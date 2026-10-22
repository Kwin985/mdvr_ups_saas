pub mod auth;
pub mod chart;
pub mod handlers;
pub mod i18n;
pub mod tree;
pub mod rbac;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::ingest::registry::Registry;
use crate::metrics::Metrics;
use crate::ota::OtaManager;
use crate::settings::SettingsStore;
use crate::storage::Store;

use handlers::AppState;
use rbac::Rbac;

pub fn build_state(
    cfg: Arc<crate::config::Config>,
    store: Store,
    registry: Arc<Registry>,
    ota: Arc<OtaManager>,
    metrics: Arc<Metrics>,
    rbac: Arc<Rbac>,
    settings: Arc<SettingsStore>,
) -> AppState {
    AppState {
        cfg,
        store,
        registry,
        ota,
        metrics,
        rbac,
        settings,
    }
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/", get(|| async { Redirect::to("/monitor") }))
        .route("/monitor", get(handlers::monitor_page))
        .route("/api/tree", get(handlers::tree_json))
        .route("/analysis", get(handlers::analysis_page))
        .route("/analysis/export", get(handlers::analysis_export))
        .route("/devices/{sn}", get(handlers::device_detail))
        .route("/devices/{sn}/panel", get(handlers::device_panel))
        .route("/devices/{sn}/live", get(handlers::device_live))
        .route("/devices/{sn}/assign", post(handlers::device_assign))
        .route("/devices/{sn}/history", get(handlers::history_partial))
        .route("/devices/{sn}/command", post(handlers::device_command))
        .route("/ota", get(handlers::ota_page))
        .route("/ota/upload", post(handlers::ota_upload))
        .route("/ota/start", post(handlers::ota_start))
        .route("/ota/{sn}/start", post(handlers::ota_start))
        .route("/ota/jobs/{id}/progress", get(handlers::ota_progress))
        .route("/admin/users", get(handlers::users_page))
        .route("/admin/users", post(handlers::user_create))
        .route("/admin/users/password", post(handlers::user_password))
        .route("/admin/users/delete", post(handlers::user_delete))
        .route("/admin/users/parent", post(handlers::user_parent))
        .route("/admin/settings", get(handlers::settings_page))
        .route("/admin/settings", post(handlers::settings_save))
        .route("/admin/restart", post(handlers::admin_restart))
        .route("/api/ota/jobs/{id}", get(handlers::ota_job_json))
        .route("/logout", post(handlers::logout))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    Router::new()
        .route(
            "/login",
            get(handlers::login_page).post(handlers::login_submit),
        )
        // 产品展示页：登录前即可访问，是登录页上的对外入口
        .route("/ups", get(handlers::ups_showcase))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_json))
        .nest_service("/static", ServeDir::new(state.cfg.static_dir.clone()))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        // 响应压缩：设备树 JSON / 页面 HTML 压缩率约 15–19 倍。
        // 公网按流量计费的部署（如 GCP e2-micro 每月 1GB 出站）必须开启。
        .layer(CompressionLayer::new())
        .with_state(state)
}

async fn healthz(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "online_devices": state.registry.online_count().await,
        "active_jobs": state.ota.active_jobs().len(),
    }))
}

async fn metrics_json(State(state): State<AppState>) -> Json<serde_json::Value> {
    let snap = state.metrics.snapshot();
    let store_stats = &state.store.stats;
    Json(serde_json::json!({
        "connections": snap.active_connections,
        "frames_total": snap.frames_total,
        "checksum_errors": snap.checksum_errors,
        "bytes_in": snap.bytes_in,
        "bytes_out": snap.bytes_out,
        "samples_ingested": snap.samples_ingested,
        "samples_written": store_stats.samples_written.load(std::sync::atomic::Ordering::Relaxed),
        "ota_started": snap.ota_started,
        "ota_succeeded": snap.ota_succeeded,
        "ota_failed": snap.ota_failed,
    }))
}

async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().as_str().to_string();
    let is_api = path.starts_with("/api/");

    let Some(token) = auth::token_from_cookie(req.headers()) else {
        return unauthenticated(is_api, &path);
    };
    let Some(claims) = auth::verify_token(&state.cfg, &token) else {
        return unauthenticated(is_api, &path);
    };
    let idle_secs = state
        .settings
        .i64("session_idle_min", state.cfg.session_idle_min)
        .clamp(1, 720)
        * 60;
    // 滑动续期：剩余有效期不足一半时补发新令牌。持续操作的人永远不会掉线，
    // 停手超过空闲窗口后令牌自然过期——这也是前端锁定页面的服务端兜底。
    let renew = claims.exp - chrono::Utc::now().timestamp() <= idle_secs / 2;
    let (username, role) = (claims.sub.clone(), claims.role.clone());
    let user = auth::AuthUser {
        username: claims.sub,
        role: claims.role,
    };
    if !state.rbac.enforce(&user.role, &path, &method) {
        tracing::warn!(user = %user.username, role = %user.role, path = %path, method = %method, "权限不足");
        if is_api {
            return (StatusCode::FORBIDDEN, "forbidden").into_response();
        }
        return handlers::forbidden_response(&user, &path, &method);
    }
    req.extensions_mut().insert(user);
    let mut resp = next.run(req).await;
    if renew
        && let Ok(fresh) = auth::create_token(&state.cfg, idle_secs, &username, &role)
        && let Ok(value) = auth::set_auth_cookie(&fresh, idle_secs).parse()
    {
        resp.headers_mut()
            .append(axum::http::header::SET_COOKIE, value);
    }
    resp
}

fn unauthenticated(is_api: bool, path: &str) -> Response {
    if is_api {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    Redirect::to(&format!("/login?next={}", urlencode(path))).into_response()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
