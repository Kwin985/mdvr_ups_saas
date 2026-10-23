use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use askama::Template;
use axum::extract::{ConnectInfo, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use chrono::{Local, TimeZone, Utc};
use serde::Deserialize;

use crate::config::Config;
use crate::ingest::registry::Registry;
use crate::metrics::Metrics;
use crate::ota::{JobProgress, OtaManager};
use crate::protocol::{Frame, cmd, codec};
use crate::settings::SettingsStore;
use crate::storage::{FirmwareRow, JobRow, SampleRow, Store};

use super::auth::{self, AuthUser};
use super::chart::{self, Series};
use super::i18n::{self, Tr};
use super::rbac::Rbac;
use super::tree::{self, UserNode};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Store,
    pub registry: Arc<Registry>,
    pub ota: Arc<OtaManager>,
    pub metrics: Arc<Metrics>,
    pub rbac: Arc<Rbac>,
    /// 系统配置（运行时可变）
    pub settings: Arc<SettingsStore>,
    /// 登录防爆破（进程内存，重启即重置）
    pub login_throttle: Arc<auth::LoginThrottle>,
}

// ---------------------------------------------------------------------------
// 错误处理
// ---------------------------------------------------------------------------

pub struct AppError(pub anyhow::Error);

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "请求处理失败");
        // 这里拿不到 SettingsStore（AppError 由 `?` 从任意错误转换而来，没有 state），
        // 所以不能走 i18n 词典。写成中英双语前缀，英文部署下也不会突然冒出中文。
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!(
                "<div class=\"toast err\">服务端错误 / Server error: {}</div>",
                self.0
            )),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// 视图模型
// ---------------------------------------------------------------------------

/// 数据分析页的单个指标统计（最小/平均/最大，已格式化并带单位）。
pub struct StatGroupView {
    pub label: String,
    pub unit: &'static str,
    /// 深色卡片配色类：ok / warn / bad / 空
    pub color: &'static str,
    pub min: String,
    pub avg: String,
    pub max: String,
}

/// 数据分析页的关键事件（电池变化点），用于图内标注与图下清单。
pub struct EventView {
    /// 本地化时间（MM-DD HH:MM）
    pub time: String,
    /// 本地化标签（充电开始 / 充满 …）
    pub label: String,
    /// 配色类：charge / discharge / full
    pub kind: String,
}

pub struct JobView {
    pub id: String,
    pub sn: String,
    pub state: String,
    pub state_label: String,
    pub state_class: String,
    pub percent: u8,
    pub sent: i64,
    pub total: i64,
    pub message: String,
    pub started: String,
    pub firmware_version: String,
    pub finished: bool,
}

pub struct SampleView {
    pub time: String,
    pub input: String,
    pub output: String,
    pub temperature: String,
    pub battery: i64,
    pub health: String,
}

pub struct FirmwareView {
    pub id: String,
    pub filename: String,
    pub version: String,
    pub size: String,
    pub sha256: String,
    pub uploaded: String,
    pub uploaded_by: String,
}

pub struct UserView {
    pub username: String,
    pub role: String,
    pub created: String,
    pub is_self: bool,
    /// 上级用户名，空表示顶级
    pub parent: String,
    /// 层级缩进深度
    pub depth: usize,
    pub devices: usize,
    /// 该用户可见的设备数（自己 + 全部下级；根管理员为全部设备）
    pub visible_devices: usize,
    /// 直接下级数量（决定能否删除）
    pub children: usize,
    /// 可否作为该用户的上级：排除自身与自己的子树，避免成环
    pub parent_choices: Vec<ParentChoice>,
}

pub struct ParentChoice {
    pub name: String,
    pub selected: bool,
}

pub struct DeviceOption {
    pub sn: String,
    pub mdvr_id: String,
    pub online: bool,
}

fn health_class(h: i64) -> &'static str {
    match h {
        0 => "ok",
        1 => "warn",
        _ => "bad",
    }
}

fn health_label(t: &Tr, h: i64) -> &'static str {
    match h {
        0 => t.tr("health.good"),
        1 => t.tr("health.fair"),
        2 => t.tr("health.poor"),
        _ => t.tr("health.replace"),
    }
}

fn fmt_ts(ms: i64) -> String {
    if ms <= 0 {
        return "—".into();
    }
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "—".into())
}

fn fmt_relative(t: &Tr, ms: i64) -> String {
    if ms <= 0 {
        return t.tr("rel.never").into();
    }
    let secs = (Utc::now().timestamp_millis() - ms) / 1000;
    if secs < 5 {
        return t.tr("rel.now").into();
    }
    let (key, n) = match secs {
        s if s < 60 => ("rel.secs", s),
        s if s < 3600 => ("rel.mins", s / 60),
        s if s < 86400 => ("rel.hours", s / 3600),
        s => ("rel.days", s / 86400),
    };
    t.trf(key, &[("n", n.to_string())])
}

fn fmt_size(bytes: i64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / 1048576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn job_view(t: &Tr, job: &JobRow, live: Option<JobProgress>) -> JobView {
    let (state, percent, sent, message) = match live {
        Some(p) => (p.state, p.percent, p.sent_packets as i64, p.message),
        None => {
            let percent = if job.total_packets > 0 {
                ((job.sent_packets * 100) / job.total_packets) as u8
            } else {
                0
            };
            (job.state.clone(), percent, job.sent_packets, job.message.clone())
        }
    };
    let state_class = match state.as_str() {
        "success" => "ok",
        "failed" => "bad",
        "aborted" => "warn",
        _ => "run",
    };
    let state_label = match state.as_str() {
        "success" => t.tr("ota_state.success"),
        "failed" => t.tr("ota_state.failed"),
        "aborted" => t.tr("ota_state.aborted"),
        "restarting" => t.tr("ota_state.restarting"),
        "running" => t.tr("ota_state.running"),
        other => other,
    };
    JobView {
        id: job.id.clone(),
        sn: job.sn.clone(),
        finished: matches!(state.as_str(), "success" | "failed" | "aborted"),
        state_label: state_label.to_string(),
        state,
        state_class: state_class.into(),
        percent,
        sent,
        total: job.total_packets,
        message,
        started: fmt_ts(job.started_at_ms),
        firmware_version: String::new(),
    }
}

// ---------------------------------------------------------------------------
// 模板
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    t: Tr,
    error: String,
    next: String,
}

/// UPS 产品展示页：**不需要登录**，作为登录页上的对外入口。
/// 只依赖系统设置里的语言与站点名，不碰任何业务数据。
#[derive(Template)]
#[template(path = "ups.html")]
struct UpsShowcaseTemplate {
    t: Tr,
    /// 本页当前语言（已归一化），模板用它高亮中/EN 切换按钮
    lang: String,
}

#[derive(Deserialize)]
pub struct UpsQuery {
    /// 本页语言覆盖（`?lang=en-US`）。
    ///
    /// 这是给客户看的公开页，不能要求对方先去后台改站点语言才能看英文版，
    /// 所以允许 URL 参数临时覆盖；不写库、不影响控制台。
    lang: Option<String>,
}

#[derive(Template)]
#[template(path = "analysis.html")]
struct AnalysisTemplate {
    t: Tr,
    i18n_js: String,
    user: AuthUser,
    active: &'static str,
    /// SN 下拉选项（仅当前用户可见的设备）
    devices: Vec<DeviceOption>,
    /// 当前分析的 SN（空 = 无可见设备）
    sn: String,
    /// 回显 yyyy-mm-dd
    from: String,
    to: String,
    /// 「SN · from ~ to」
    range_label: String,
    count: i64,
    stats: Vec<StatGroupView>,
    /// 电池关键事件（充电开始 / 放电开始 / 充满），图内标线 + 图下清单共用
    events: Vec<EventView>,
    battery_chart: String,
    voltage_chart: String,
    temp_chart: String,
}

/// 设备详情片段：由 /monitor 右侧面板与 HTMX 局部刷新共用。
#[derive(Template)]
#[template(path = "device_panel.html")]
#[allow(dead_code)]
struct DevicePanel {
    t: Tr,
    i18n_js: String,
    sn: String,
    mdvr_id: String,
    owner: String,
    owner_label: String,
    group_name: String,
    owners: Vec<String>,
    can_assign: bool,
    online: bool,
    busy: bool,
    firmware: String,
    address: i64,
    baudrate: i64,
    report_interval: i64,
    /// 波特率档位下拉（预渲染 HTML，当前档位 selected）
    baud_options: String,
    battery: i64,
    input_voltage: String,
    output_voltage: String,
    temperature: String,
    health: String,
    health_class: String,
    last_seen: String,
    uptime: String,
    battery_chart: String,
    temp_chart: String,
    voltage_chart: String,
    jobs: Vec<JobView>,
    remote: String,
    can_control: bool,
    can_ota: bool,
}

#[derive(Template)]
#[template(path = "history.html")]
#[allow(dead_code)]
struct HistoryTemplate {
    t: Tr,
    i18n_js: String,
    sn: String,
    chart: String,
    /// 「设备 X · 当前窗口：近 N 小时」
    foot: String,
    samples: Vec<SampleView>,
    avg_battery: String,
    min_battery: i64,
    max_battery: i64,
    avg_temp: String,
    temp_range: String,
    avg_input: String,
    avg_output: String,
    count: i64,
}

#[derive(Template)]
#[template(path = "ota.html")]
struct OtaTemplate {
    t: Tr,
    i18n_js: String,
    user: AuthUser,
    active: &'static str,
    firmware: Vec<FirmwareView>,
    devices: Vec<DeviceOption>,
    jobs: Vec<JobView>,
    flash: String,
    error: String,
    can_manage: bool,
    /// 从设备面板跳转时预选的目标设备
    selected_sn: String,
}

#[derive(Template)]
#[template(path = "ota_progress.html")]
struct ProgressTemplate {
    t: Tr,
    job: JobView,
}

#[derive(Template)]
#[template(path = "users.html")]
struct UsersTemplate {
    t: Tr,
    i18n_js: String,
    user: AuthUser,
    active: &'static str,
    /// 根管理员用户名（层级根节点）
    root: String,
    users: Vec<UserView>,
    parent_options: Vec<String>,
    flash: String,
    error: String,
    subtitle: String,
    scope_admin: String,
    hierarchy_body: String,
    unassigned_body: String,
}

#[derive(Template)]
#[template(path = "forbidden.html")]
#[allow(dead_code)]
struct ForbiddenTemplate {
    t: Tr,
    i18n_js: String,
    user: AuthUser,
    active: &'static str,
    path: String,
    method: String,
    body: String,
}

#[derive(Template)]
#[template(path = "toast.html")]
struct ToastTemplate {
    ok: bool,
    message: String,
}

// ---------------------------------------------------------------------------
// 页面
// ---------------------------------------------------------------------------

pub async fn login_page(State(state): State<AppState>, Query(q): Query<LoginQuery>) -> Response {
    LoginTemplate {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        error: String::new(),
        next: q.next.unwrap_or_else(|| "/analysis".into()),
    }
    .render()
    .map(|h| Html(h).into_response())
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[derive(Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

/// 产品展示页，公开访问。放在登录页之外，方便销售/客户直接看型号与接线图。
pub async fn ups_showcase(
    State(state): State<AppState>,
    Query(q): Query<UpsQuery>,
) -> Response {
    // 只接受白名单里的语言，非法值静默回落到站点设置，避免 `?lang=xx` 打出乱码页
    let lang = q
        .lang
        .filter(|l| crate::settings::SUPPORTED_LANGS.iter().any(|(k, _)| k == l))
        .unwrap_or_else(|| state.settings.lang());
    UpsShowcaseTemplate {
        t: Tr::from(lang.clone()).with_site(state.settings.str("site_title")),
        lang,
    }
    .render()
    .map(|h| Html(h).into_response())
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
    next: Option<String>,
}

pub async fn login_submit(
    State(state): State<AppState>,
    // 需要 main.rs 用 into_make_service_with_connect_info 起服务；
    // 全项目只有这一个服务入口，拿不到 IP 时 axum 会明确报错而不是静默降级。
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Form(form): Form<LoginForm>,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let throttle_key = format!("ip:{}", peer.ip());
    if !state.login_throttle.allow(&throttle_key) {
        tracing::warn!(key = %throttle_key, "登录被防爆破拦截");
        return LoginTemplate {
            t: t.clone(),
            error: t.tr("login.throttled").into(),
            next: sanitize_next(form.next.as_deref()),
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response())
        .into_response();
    }
    let next = sanitize_next(form.next.as_deref());
    let user = match state.store.user(&form.username).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "查询用户失败");
            None
        }
    };
    let Some(user) = user else {
        state.login_throttle.record_failure(&throttle_key);
        return LoginTemplate {
            t: t.clone(),
            error: t.tr("login.bad_credentials").into(),
            next,
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        .into_response();
    };
    if !auth::verify_password(&user.password_hash, &form.password) {
        state.login_throttle.record_failure(&throttle_key);
        return LoginTemplate {
            t: t.clone(),
            error: t.tr("login.bad_credentials").into(),
            next,
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        .into_response();
    }
    // 会话长度 = 空闲超时窗口（分钟）。持续操作时中间件会滑动续期，
    // 所以这里不用再单独给一个"登录有效期"。
    let idle_secs = state
        .settings
        .i64("session_idle_min", state.cfg.session_idle_min)
        .clamp(1, 720)
        * 60;
    let Ok(token) = auth::create_token(&state.cfg, idle_secs, &user.username, &user.role) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    state.login_throttle.record_success(&throttle_key);
    state.store.audit(&user.username, "auth.login", "-", "登录成功");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        auth::set_auth_cookie(&token, idle_secs).parse().unwrap(),
    );
    headers.insert(header::LOCATION, next.parse().unwrap());
    (StatusCode::SEE_OTHER, headers).into_response()
}

pub async fn logout(State(state): State<AppState>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        auth::clear_auth_cookie().parse().unwrap(),
    );
    headers.insert(header::LOCATION, "/login".parse().unwrap());
    let _ = state;
    (StatusCode::SEE_OTHER, headers).into_response()
}

// ---------------------------------------------------------------------------
// 数据分析（/analysis）：按 SN 分析 UPS 历史数据
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AnalysisQuery {
    pub sn: Option<String>,
    /// 预设天数（近 7 / 30 天）；显式给出 from/to 时以日期为准
    pub days: Option<i64>,
    /// yyyy-mm-dd（含当天 00:00）
    pub from: Option<String>,
    /// yyyy-mm-dd（含当天 23:59:59）
    pub to: Option<String>,
}

/// 原始样本抓取上限：DESC 取最近的 LIMIT 条，超过视为超长窗口。
const ANALYSIS_RAW_LIMIT: i64 = 200_000;
/// 每条曲线最多绘制的点数，超出按等步长抽稀（统计仍用全量样本）。
const ANALYSIS_MAX_POINTS: usize = 400;
/// 单次分析窗口上限（天），防止误选超大区间拖垮查询。
const ANALYSIS_MAX_SPAN_DAYS: i64 = 366;

pub async fn analysis_page(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<AnalysisQuery>,
) -> Result<Html<String>, AppError> {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));

    // ---- SN 下拉：仅当前用户可见的设备，在线的排前面 ----
    let visible = visible_device_sns(&state, &user).await;
    let online: std::collections::HashSet<String> = state
        .registry
        .list()
        .await
        .into_iter()
        .map(|d| d.sn)
        .collect();
    let mut devices: Vec<DeviceOption> = state
        .store
        .devices()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|d| visible.contains(&d.sn))
        .map(|d| DeviceOption {
            online: online.contains(&d.sn),
            sn: d.sn,
            mdvr_id: d.mdvr_id,
        })
        .collect();
    devices.sort_by(|a, b| (!a.online, &a.sn).cmp(&(!b.online, &b.sn)));

    // 请求的 SN 必须可见，否则回落到第一台；时间范围日期优先，其次 days 预设，默认近 7 天
    let fallback_sn = devices.first().map(|d| d.sn.clone());
    let (sn, from_ms, to_ms, from_date, to_date) =
        resolve_analysis_target(&q, &visible, fallback_sn.as_deref());
    let from_str = from_date.format("%Y-%m-%d").to_string();
    let to_str = to_date.format("%Y-%m-%d").to_string();

    // ---- 取原始样本（统计用全量） ----
    let mut samples = if sn.is_empty() {
        Vec::new()
    } else {
        let mut v = state
            .store
            .history(crate::storage::HistoryQuery {
                sn: sn.clone(),
                from_ms,
                to_ms,
                limit: ANALYSIS_RAW_LIMIT,
            })
            .await
            .unwrap_or_default();
        v.reverse(); // 存储 DESC 返回 → 时间升序
        v
    };

    let count = samples.len() as i64;
    let inputs: Vec<f64> = samples.iter().map(|s| s.input_voltage).collect();
    let outputs: Vec<f64> = samples.iter().map(|s| s.output_voltage).collect();
    let temps: Vec<f64> = samples.iter().map(|s| s.temperature).collect();
    let batteries: Vec<f64> = samples.iter().map(|s| s.battery as f64).collect();

    // 全量电量序列（带时间戳，升序）用于识别关键事件，与抽稀曲线共用
    let battery_full: Vec<(i64, f64)> = samples
        .iter()
        .map(|s| (s.ts_ms, s.battery as f64))
        .collect();
    let raw_events = detect_battery_events(&battery_full);
    let chart_events: Vec<chart::ChartEvent> = raw_events
        .iter()
        .map(|(t, k)| chart::ChartEvent { t: *t, kind: *k })
        .collect();
    let event_views: Vec<EventView> = raw_events
        .iter()
        .map(|(ts, k)| {
            let (label, kind) = match k {
                chart::EventKind::Charge => (t.tr("analysis.evt_charge_start"), "charge"),
                chart::EventKind::Discharge => (t.tr("analysis.evt_discharge_start"), "discharge"),
                chart::EventKind::Full => (t.tr("analysis.evt_full"), "full"),
            };
            EventView {
                time: chart::fmt_time(*ts),
                label: label.to_string(),
                kind: kind.to_string(),
            }
        })
        .collect();

    let stats = vec![
        stat_group(&t, "analysis.input_v", "V", "ok", &inputs),
        stat_group(&t, "analysis.output_v", "V", "", &outputs),
        stat_group(&t, "analysis.temp", "℃", "warn", &temps),
        stat_group(&t, "analysis.battery", "%", "ok", &batteries),
    ];

    // ---- 曲线（等步长抽稀，保持三组序列时间对齐） ----
    let keep: Vec<usize> = decimate_indices(samples.len(), ANALYSIS_MAX_POINTS);
    let picked: Vec<&SampleRow> = keep.into_iter().filter_map(|i| samples.get(i)).collect();
    let battery_points: Vec<(i64, f64)> = picked.iter().map(|s| (s.ts_ms, s.battery as f64)).collect();
    let input_points: Vec<(i64, f64)> = picked.iter().map(|s| (s.ts_ms, s.input_voltage)).collect();
    let output_points: Vec<(i64, f64)> = picked.iter().map(|s| (s.ts_ms, s.output_voltage)).collect();
    let temp_points: Vec<(i64, f64)> = picked.iter().map(|s| (s.ts_ms, s.temperature)).collect();
    samples.clear(); // 统计已算完，释放内存

    let battery_chart = chart::line_chart_events(
        &Series {
            label: t.tr("analysis.battery").into(),
            unit: "%".into(),
            color: "#4fc3f7".into(),
            points: battery_points,
            // 电量为整数百分比：整刻度 + 固定 0~100 物理范围
            tick_decimals: Some(0),
            step: false,
            fixed_range: Some((0.0, 100.0)),
        },
        &chart_events,
        t.tr("analysis.no_data"),
    );
    let voltage_chart = chart::line_chart_multi(
        &[
            Series {
                label: t.tr("analysis.input_v").into(),
                unit: "V".into(),
                color: "#059669".into(),
                points: input_points,
                tick_decimals: None,
                step: false,
                fixed_range: None,
            },
            Series {
                label: t.tr("analysis.output_v").into(),
                unit: "V".into(),
                color: "#1d4ed8".into(),
                points: output_points,
                tick_decimals: None,
                step: false,
                fixed_range: None,
            },
        ],
        &[],
        t.tr("analysis.no_data"),
    );
    let temp_chart = chart::line_chart(
        &Series {
            label: t.tr("analysis.temp").into(),
            unit: "℃".into(),
            color: "#f5a524".into(),
            points: temp_points,
            // 设备只上报整度：整度刻度 + 方波
            tick_decimals: Some(0),
            step: true,
            fixed_range: None,
        },
        t.tr("analysis.no_data"),
    );

    AnalysisTemplate {
        t: t.clone(),
        i18n_js: i18n::js_dict(&state.settings.lang()),
        user,
        active: "analysis",
        devices,
        range_label: format!("{sn} · {from_str} ~ {to_str}"),
        sn,
        from: from_str,
        to: to_str,
        count,
        stats,
        events: event_views,
        battery_chart,
        voltage_chart,
        temp_chart,
    }
    .render()
    .map(Html)
    .map_err(|e| AppError(anyhow::anyhow!(e)))
}

/// 数据分析导出：把所选 SN / 时间范围内的历史样本写成 CSV 保存到桌面。
/// 返回 JSON：`{ ok, message, path? }`，前端据此提示用户文件位置。
pub async fn analysis_export(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<AnalysisQuery>,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));

    let visible = visible_device_sns(&state, &user).await;
    let fallback_sn = state
        .store
        .devices()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|d| visible.contains(&d.sn))
        .map(|d| d.sn);
    let (sn, from_ms, to_ms, _from_date, _to_date) =
        resolve_analysis_target(&q, &visible, fallback_sn.as_deref());

    if sn.is_empty() {
        return json_message(false, t.tr("analysis.export_empty"));
    }

    let mut samples = state
        .store
        .history(crate::storage::HistoryQuery {
            sn: sn.clone(),
            from_ms,
            to_ms,
            limit: ANALYSIS_RAW_LIMIT,
        })
        .await
        .unwrap_or_default();
    if samples.is_empty() {
        return json_message(false, t.tr("analysis.export_empty"));
    }
    samples.reverse(); // 存储按 DESC 返回 → CSV 按时间从早到晚

    // CSV 表头（带单位，便于直接用表格软件打开）。首列即可读的本地时间，
    // 不再输出 13 位毫秒时间戳；开头加 UTF-8 BOM，Excel 打开中文（健康状态等）不乱码。
    let mut csv = String::from(
        "\u{feff}local_time,sn,input_voltage_v,output_voltage_v,temperature_c,battery_pct,load_pct,health,firmware\n",
    );
    for s in &samples {
        csv.push_str(&format!(
            "{},{},{:.2},{:.2},{:.1},{},{},{},{}\n",
            fmt_local_datetime(s.ts_ms),
            csv_field(&sn),
            s.input_voltage,
            s.output_voltage,
            s.temperature,
            s.battery,
            s.load_percent,
            s.health,
            csv_field(&s.firmware),
        ));
    }

    // 文件名：SN_开始_结束.csv（直接体现设备与时间段）
    let file_name = format!(
        "{}_{}_{}.csv",
        sanitize_filename(&sn),
        fmt_compact(from_ms),
        fmt_compact(to_ms)
    );
    // 直接以附件流回浏览器。以前是写到**服务器**桌面再返回路径，只有坐在服务器
    // 前面的人拿得到文件；远程访问时用户点导出等于什么也没发生。
    csv_response(&file_name, csv)
}

/// 以 `text/csv` 附件形式返回，文件名同时给出 ASCII 回退与 RFC 5987 编码，
/// 这样 SN 里带中文/空格时各浏览器都能得到正确名字。
fn csv_response(file_name: &str, body: String) -> Response {
    let ascii_name: String = file_name
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' { c } else { '_' })
        .collect();
    let encoded = percent_encode(file_name);
    let disposition = format!(
        "attachment; filename=\"{ascii_name}\"; filename*=UTF-8''{encoded}"
    );
    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(body))
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "构造 CSV 响应失败");
            (StatusCode::INTERNAL_SERVER_ERROR, "export failed").into_response()
        }
    }
}

/// RFC 3986 百分号编码（只保留 unreserved 字符），用于 `filename*=UTF-8''`。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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

/// 本地时间 `YYYY-MM-DD HH:MM:SS`。
fn fmt_local_datetime(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// 文件名用紧凑时间 `YYYY-MM-DD_HHMM`。
fn fmt_compact(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d_%H%M").to_string())
        .unwrap_or_default()
}

/// 去掉 Windows / POSIX 文件名非法字符，避免写文件失败。
fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "ups".to_string()
    } else {
        trimmed.to_string()
    }
}

/// CSV 字段转义：含逗号/引号/换行时加双引号，内部引号翻倍。
fn csv_field(v: &str) -> String {
    if v.contains(',') || v.contains('"') || v.contains('\n') || v.contains('\r') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn json_message(ok: bool, message: &str) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        axum::Json(serde_json::json!({ "ok": ok, "message": message })),
    )
        .into_response()
}

/// 解析分析页 / 导出共用的目标 SN 与时间范围。
/// `visible` 为当前用户可见的 SN 集合，`fallback_sn` 在请求 SN 不可见时兜底。
/// 返回 `(sn, from_ms, to_ms, from_date, to_date)`；时间范围日期优先、其次 days 预设、默认近 7 天。
fn resolve_analysis_target(
    q: &AnalysisQuery,
    visible: &HashSet<String>,
    fallback_sn: Option<&str>,
) -> (String, i64, i64, chrono::NaiveDate, chrono::NaiveDate) {
    let sn = q
        .sn
        .as_ref()
        .filter(|s| !s.is_empty() && visible.contains(*s))
        .cloned()
        .or_else(|| fallback_sn.map(|s| s.to_string()))
        .unwrap_or_default();

    let today = Local::now().date_naive();
    let to_date = parse_date(&q.to).unwrap_or(today);
    let days = q.days.unwrap_or(7).clamp(1, ANALYSIS_MAX_SPAN_DAYS);
    let from_date = parse_date(&q.from).unwrap_or_else(|| {
        to_date
            .checked_sub_signed(chrono::Duration::days(days - 1))
            .unwrap_or(to_date)
    });
    let (from_date, to_date) = if from_date > to_date {
        (to_date, from_date)
    } else {
        (from_date, to_date)
    };
    // 超长窗口收缩到上限
    let from_date = to_date
        .checked_sub_signed(chrono::Duration::days(ANALYSIS_MAX_SPAN_DAYS))
        .map(|min| from_date.max(min))
        .unwrap_or(from_date);

    let from_ms = local_ms(from_date, false).unwrap_or(0);
    let to_ms = local_ms(to_date, true).unwrap_or(i64::MAX);
    (sn, from_ms, to_ms, from_date, to_date)
}

fn parse_date(s: &Option<String>) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s.as_deref()?.trim(), "%Y-%m-%d").ok()
}

/// 本地时区的当日 00:00:00（`end=false`）或 23:59:59（`end=true`）的毫秒时间戳。
fn local_ms(d: chrono::NaiveDate, end: bool) -> Option<i64> {
    use chrono::TimeZone;
    let naive = if end {
        d.and_hms_opt(23, 59, 59)?
    } else {
        d.and_hms_opt(0, 0, 0)?
    };
    Local.from_local_datetime(&naive).single().map(|dt| dt.timestamp_millis())
}

/// 一组样本的 (最小, 平均, 最大)；空集返回 (0, 0, 0)。
fn metric_stats(vals: &[f64]) -> (f64, f64, f64) {
    if vals.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let avg = vals.iter().sum::<f64>() / vals.len() as f64;
    (min, avg, max)
}

fn stat_group(t: &Tr, label_key: &str, unit: &'static str, color: &'static str, vals: &[f64]) -> StatGroupView {
    let (min, avg, max) = metric_stats(vals);
    StatGroupView {
        label: t.tr(label_key).to_string(),
        unit,
        color,
        min: format!("{min:.2} {unit}"),
        avg: format!("{avg:.2} {unit}"),
        max: format!("{max:.2} {unit}"),
    }
}

/// 等步长抽稀的下标：n 个点最多保留 max_points 个（含首点）。
fn decimate_indices(n: usize, max_points: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let stride = n.div_ceil(max_points).max(1);
    (0..n).step_by(stride).collect()
}

/// 从电量时间序列（升序）识别关键事件：
/// * `Charge`    —— 电量转入持续上升（开始充电）
/// * `Discharge` —— 电量转入持续下降（开始放电）
/// * `Full`      —— 电量越过 ~99%（充满）
///
/// 先对序列做「拖尾移动平均」平滑以去除上报抖动（真实 UPS 上报常有 ±1~2% 的小波动），
/// 再在平滑序列上用「居中斜率 + 状态机」判定充放电状态：斜率显著为正→充电、
/// 显著为负→放电、否则空闲；状态切换时记一次开始事件。平滑让缓慢的浮充趋势也能
/// 被识别为充电，同时把噪声小波过滤掉，避免把瞬时抖动误判成充放电。
/// 充满为独立的越界判定（要求平滑值也确认，避免单点尖峰误报）。
/// 最终对间隔过近的事件去重（< 15 分钟），最多保留 24 个，避免曲线被标线淹没。
fn detect_battery_events(series: &[(i64, f64)]) -> Vec<(i64, chart::EventKind)> {
    use chart::EventKind;
    let n = series.len();
    if n < 5 {
        return Vec::new();
    }
    // 拖尾移动平均窗口：随数据量自适应（约覆盖数分钟~半小时），既去抖又不抹掉趋势
    let k = (n / 300).clamp(5, 60);
    let sm: Vec<f64> = (0..n)
        .map(|i| {
            let lo = i.saturating_sub(k - 1);
            let s: f64 = series[lo..=i].iter().map(|p| p.1).sum();
            s / ((i - lo + 1) as f64)
        })
        .collect();
    const N: usize = 10; // 平滑序列上的居中状态窗口（点数）
    const SLOPE_EPS: f64 = 1e-8; // 斜率阈值（%/ms），远低于真实充放电、高于平滑残差
    let state_at = |i: usize| -> i8 {
        if i < N || i + N >= n {
            0
        } else {
            let dt = series[i + N].0 - series[i - N].0;
            if dt <= 0 {
                0
            } else {
                let s = (sm[i + N] - sm[i - N]) / dt as f64;
                if s > SLOPE_EPS {
                    1
                } else if s < -SLOPE_EPS {
                    -1
                } else {
                    0
                }
            }
        }
    };
    let mut out: Vec<(i64, EventKind)> = Vec::new();
    // 起点若已近满，则视为已充满，避免一开始误报「充满」
    let mut charged = series.first().map(|s| s.1 >= 99.0).unwrap_or(false);
    let mut prev_state: i8 = 0;
    for i in 0..n {
        let st = state_at(i);
        let (t, v) = series[i];
        // 充满：原始值越过 99% 且平滑值确认（≥98，排除单点尖峰）
        if !charged && v >= 99.0 && sm[i] >= 98.0 {
            charged = true;
            out.push((t, EventKind::Full));
        }
        if charged && v < 95.0 {
            charged = false;
        }
        // 进入充电态（空闲/放电 → 充电）：记一次充电开始
        if st == 1 && prev_state != 1 {
            out.push((t, EventKind::Charge));
        }
        // 进入放电态（空闲/充电 → 放电）：记一次放电开始
        if st == -1 && prev_state != -1 {
            out.push((t, EventKind::Discharge));
        }
        prev_state = st;
    }
    // 去重：与上一个事件间隔 < 15 分钟则跳过；最多保留 24 个
    let min_gap: i64 = 15 * 60_000;
    let mut kept: Vec<(i64, EventKind)> = Vec::new();
    for (t, k) in out {
        if let Some((last_t, _)) = kept.last()
            && t - *last_t < min_gap
        {
            continue;
        }
        kept.push((t, k));
        if kept.len() >= 24 {
            break;
        }
    }
    kept
}

/// 旧详情页地址保留为跳转，统一走 /monitor 的树 + 右侧面板。
pub async fn device_detail(Path(sn): Path<String>) -> Response {
    Redirect::to(&format!("/monitor?sn={}", crate::web::urlencode(&sn))).into_response()
}

pub async fn device_panel(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sn): Path<String>,
) -> Result<Response, AppError> {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    if !can_see_device(&state, &user, &sn).await {
        return Ok((StatusCode::NOT_FOUND, Html(format!("<div class='empty-hint'>{}</div>", t.tr("cmd.err_device")))).into_response());
    }
    let rows = state.store.device(&sn).await.unwrap_or_default();
    let Some(row) = rows else {
        return Ok((StatusCode::NOT_FOUND, Html(format!("<div class='empty-hint'>{}</div>", t.tr("cmd.err_device")))).into_response());
    };
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let live = state.registry.get(&sn).await;
    let latest = state.store.latest(&sn).await.ok().flatten();
    let now = Utc::now().timestamp_millis();
    let from = now - 24 * 3600 * 1000;
    let buckets = state
        .store
        .buckets(&sn, from, now, 5 * 60 * 1000)
        .await
        .unwrap_or_default();

    let battery_points: Vec<(i64, f64)> =
        buckets.iter().map(|b| (b.ts_ms, b.avg_battery)).collect();
    let temp_points: Vec<(i64, f64)> = buckets.iter().map(|b| (b.ts_ms, b.avg_temp)).collect();
    let voltage_points: Vec<(i64, f64)> =
        buckets.iter().map(|b| (b.ts_ms, b.avg_input)).collect();

    let jobs = state
        .store
        .jobs_for_sn(&sn, 50)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|j| job_view(&t, &j, state.ota.progress(&j.id)))
        .collect();

    let can_control = state.rbac.enforce(&user.role, &format!("/devices/{sn}/command"), "POST");
    let can_ota = state.rbac.enforce(&user.role, &format!("/ota/{sn}/start"), "POST");

    // 当前波特率对应协议档位，生成下拉选项（与单机版一致的常用档位）
    let baud_index = crate::protocol::codec::BAUD_LEVELS
        .iter()
        .find(|(_, rate)| *rate as i64 == row.baudrate)
        .map(|(i, _)| *i)
        .unwrap_or(4);
    let baud_options: String = crate::protocol::codec::BAUD_LEVELS
        .iter()
        .map(|(idx, rate)| {
            format!(
                r#"<option value="{idx}"{}>{rate}</option>"#,
                if *idx == baud_index { " selected" } else { "" }
            )
        })
        .collect();

    let all_users = state.store.users().await.unwrap_or_default();
    DevicePanel {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        i18n_js: i18n::js_dict(&state.settings.lang()),
        sn: sn.clone(),
        mdvr_id: row.mdvr_id.clone(),
        owner: row.owner.clone(),
        owner_label: if row.owner.is_empty() {
            t.tr("common.unassigned").to_string()
        } else {
            row.owner.clone()
        },
        group_name: row.group_name.clone(),
        owners: all_users.iter().map(|u| u.username.clone()).collect(),
        can_assign: state.rbac.enforce(&user.role, "/devices/*/assign", "POST"),
        online: live.is_some(),
        busy: live.as_ref().map(|l| l.is_busy()).unwrap_or(false),
        firmware: latest
            .as_ref()
            .map(|s| s.firmware.clone())
            .unwrap_or(row.firmware_version.clone()),
        address: row.address,
        baudrate: row.baudrate,
        report_interval: row.report_interval,
        baud_options,
        battery: latest.as_ref().map(|s| s.battery).unwrap_or(0),
        input_voltage: latest
            .as_ref()
            .map(|s| format!("{:.1}", s.input_voltage))
            .unwrap_or_else(|| "—".into()),
        output_voltage: latest
            .as_ref()
            .map(|s| format!("{:.1}", s.output_voltage))
            .unwrap_or_else(|| "—".into()),
        temperature: latest
            .as_ref()
            .map(|s| format!("{:.1}", s.temperature))
            .unwrap_or_else(|| "—".into()),
        health: latest
            .as_ref()
            .map(|s| health_label(&t, s.health).to_string())
            .unwrap_or_else(|| t.tr("common.unknown").into()),
        health_class: latest
            .as_ref()
            .map(|s| health_class(s.health).to_string())
            .unwrap_or_else(|| "warn".into()),
        last_seen: fmt_relative(
            &t,
            live.as_ref()
                .map(|l| l.last_seen_ms())
                .unwrap_or(row.last_seen_ms),
        ),
        uptime: live
            .as_ref()
            .map(|l| format!("{} {}", l.uptime_secs() / 60, t.tr("common.minutes")))
            .unwrap_or_else(|| "—".into()),
        battery_chart: chart::line_chart(
            &Series {
                label: t.tr("chart.battery").into(),
                unit: "%".into(),
                color: "#4fc3f7".into(),
                points: battery_points,
                tick_decimals: None,
                step: false,
                fixed_range: None,
            },
            t.tr("chart.empty_battery"),
        ),
        temp_chart: chart::line_chart(
            &Series {
                label: t.tr("chart.temp").into(),
                unit: "℃".into(),
                color: "#f5a524".into(),
                points: temp_points,
                // 设备只上报整度：刻度取整，方波表示「保持到下一次上报」
                tick_decimals: Some(0),
                step: true,
                fixed_range: None,
            },
            t.tr("chart.empty_temp"),
        ),
        voltage_chart: chart::line_chart(
            &Series {
                label: t.tr("chart.voltage").into(),
                unit: "V".into(),
                color: "#2dd4bf".into(),
                points: voltage_points,
                tick_decimals: None,
                step: false,
                fixed_range: None,
            },
            t.tr("chart.empty_voltage"),
        ),
        jobs,
        remote: live
            .as_ref()
            .map(|l| l.remote.to_string())
            .unwrap_or_else(|| "—".into()),
        can_control,
        can_ota,
    }
    .render()
    .map(Html)
    .map(|h| h.into_response())
    .map_err(|e| AppError(anyhow::anyhow!(e)))
}

fn sample_view(t: &Tr, s: SampleRow) -> SampleView {
    SampleView {
        time: fmt_ts(s.ts_ms),
        input: format!("{:.1}", s.input_voltage),
        output: format!("{:.1}", s.output_voltage),
        temperature: format!("{:.1}", s.temperature),
        battery: s.battery,
        health: health_label(t, s.health).to_string(),
    }
}

/// 设备面板底部固定展示的时间窗口（小时）。按 SN 的任意区间分析请用数据分析页。
const HISTORY_WINDOW_HOURS: i64 = 24;

/// 最近 `HISTORY_WINDOW_HOURS` 小时的区间 `(from, to)`。
fn recent_window(now: i64) -> (i64, i64) {
    (now - HISTORY_WINDOW_HOURS * 3600 * 1000, now)
}

/// 本地时间 `YYYY-MM-DD HH:MM`。
fn fmt_dt_minute(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

pub async fn history_partial(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sn): Path<String>,
) -> Result<Html<String>, AppError> {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    if !can_see_device(&state, &user, &sn).await {
        return Ok(Html(format!(
            "<div class='empty-hint'>{}</div>",
            t.tr("cmd.err_device")
        )));
    }
    let now = Utc::now().timestamp_millis();
    let (from, to) = recent_window(now);
    let span = (to - from).max(60_000);
    let bucket = (span / 180).max(60_000);
    let buckets = state
        .store
        .buckets(&sn, from, to, bucket)
        .await
        .unwrap_or_default();
    let samples = state
        .store
        .history(crate::storage::HistoryQuery {
            sn: sn.clone(),
            from_ms: from,
            to_ms: to,
            limit: 50,
        })
        .await
        .unwrap_or_default();

    let points: Vec<(i64, f64)> = buckets.iter().map(|b| (b.ts_ms, b.avg_battery)).collect();
    let avg_battery = if buckets.is_empty() {
        "—".to_string()
    } else {
        format!(
            "{:.1}%",
            buckets.iter().map(|b| b.avg_battery).sum::<f64>() / buckets.len() as f64
        )
    };
    let avg_temp = if buckets.is_empty() {
        "—".to_string()
    } else {
        format!(
            "{:.1} ℃",
            buckets.iter().map(|b| b.avg_temp).sum::<f64>() / buckets.len() as f64
        )
    };
    let avg_input = if buckets.is_empty() {
        "—".to_string()
    } else {
        format!(
            "{:.2} V",
            buckets.iter().map(|b| b.avg_input).sum::<f64>() / buckets.len() as f64
        )
    };
    let avg_output = if buckets.is_empty() {
        "—".to_string()
    } else {
        format!(
            "{:.2} V",
            buckets.iter().map(|b| b.avg_output).sum::<f64>() / buckets.len() as f64
        )
    };
    let temp_range = if buckets.is_empty() {
        "—".to_string()
    } else {
        let min = buckets
            .iter()
            .map(|b| b.avg_temp)
            .fold(f64::INFINITY, f64::min);
        let max = buckets
            .iter()
            .map(|b| b.avg_temp)
            .fold(f64::NEG_INFINITY, f64::max);
        format!("{min:.1}–{max:.1} ℃")
    };
    let count = buckets.iter().map(|b| b.samples).sum::<i64>();
    let foot = t.trf(
        "history.foot_24h",
        &[
            ("sn", sn.clone()),
            ("from", fmt_dt_minute(from)),
            ("to", fmt_dt_minute(to)),
        ],
    );

    HistoryTemplate {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        i18n_js: i18n::js_dict(&state.settings.lang()),
        sn: sn.clone(),
        chart: chart::line_chart(
            &Series {
                label: t.tr("chart.battery").into(),
                unit: "%".into(),
                color: "#4fc3f7".into(),
                points,
                tick_decimals: None,
                step: false,
                fixed_range: None,
            },
            t.tr("chart.empty_window"),
        ),
        samples: samples.into_iter().map(|s| sample_view(&t, s)).collect(),
        avg_battery,
        min_battery: buckets.iter().map(|b| b.min_battery).min().unwrap_or(0),
        max_battery: buckets.iter().map(|b| b.max_battery).max().unwrap_or(0),
        avg_temp,
        temp_range,
        avg_input,
        avg_output,
        count,
        foot,
    }
    .render()
    .map(Html)
    .map_err(|e| AppError(anyhow::anyhow!(e)))
}

// ---------------------------------------------------------------------------
// 设备命令
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CommandForm {
    pub action: String,
    pub value: Option<String>,
}

pub async fn device_command(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sn): Path<String>,
    Form(form): Form<CommandForm>,
) -> Response {
    if !can_see_device(&state, &user, &sn).await {
        return toast_err(Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")).tr("cmd.err_device"));
    }
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let frame = match build_command(&t, &form) {
        Ok(f) => f,
        Err(e) => {
            return ToastTemplate {
                ok: false,
                message: e,
            }
            .render()
            .map(|h| Html(h).into_response())
            .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
            .into_response();
        }
    };
    let label = crate::protocol::codec::command_name(frame.command).to_string();
    match state.registry.send(&sn, frame, false).await {
        Ok(()) => {
            state.store.audit(
                &user.username,
                "device.command",
                &sn,
                &format!("{label} {}", form.value.clone().unwrap_or_default()),
            );
            ToastTemplate {
                ok: true,
                message: format!("{label}{}", t.tr("cmd.sent")),
            }
        }
        Err(e) => ToastTemplate {
            ok: false,
            message: e.to_string(),
        },
    }
    .render()
    .map(|h| Html(h).into_response())
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    .into_response()
}

fn build_command(t: &Tr, form: &CommandForm) -> Result<Frame, String> {
    let value = form.value.clone().unwrap_or_default();
    let frame = match form.action.as_str() {
        // 文档没有「查询状态」命令，状态由设备按推送频率主动上报；
        // 这里用「请求版本」代替查询，返回的 V 帧会刷新固件版本。
        "query" | "version" => Frame::new(codec::DEFAULT_ADDRESS, cmd::QUERY_VERSION, Vec::new()),
        "restart" => Frame::new(codec::DEFAULT_ADDRESS, cmd::RESTART, Vec::new()),
        "set_address" => {
            let addr: u8 = value
                .trim()
                .parse()
                .map_err(|_| t.tr("cmd.err_addr").to_string())?;
            if addr > 247 {
                return Err(t.tr("cmd.err_addr").to_string());
            }
            Frame::new(codec::DEFAULT_ADDRESS, cmd::SET_ADDRESS, vec![addr])
        }
        "set_baudrate" => {
            let idx: u8 = value
                .trim()
                .parse()
                .map_err(|_| t.tr("cmd.err_baud").to_string())?;
            if idx > 9 {
                return Err(t.tr("cmd.err_baud").to_string());
            }
            Frame::new(codec::DEFAULT_ADDRESS, cmd::SET_BAUDRATE, codec::baud_para(idx).to_vec())
        }
        "set_interval" => {
            let secs: u16 = value
                .trim()
                .parse()
                .map_err(|_| t.tr("cmd.err_interval").to_string())?;
            if !(1..=3600).contains(&secs) {
                return Err(t.tr("cmd.err_interval").to_string());
            }
            Frame::new(
                codec::DEFAULT_ADDRESS,
                cmd::SET_REPORT_INTERVAL,
                codec::interval_para(secs).to_vec(),
            )
        }
        "set_sn" => {
            let sn = value.trim();
            if sn.is_empty() || sn.len() > codec::STATUS_A_SN_LEN {
                return Err(t.tr("cmd.err_sn").to_string());
            }
            let mut payload = vec![0x20u8; codec::STATUS_A_SN_LEN];
            payload[..sn.len()].copy_from_slice(sn.as_bytes());
            Frame::new(codec::DEFAULT_ADDRESS, cmd::SET_SN, payload)
        }
        other => return Err(format!("{} {other}", t.tr("cmd.err_unknown"))),
    };
    Ok(frame)
}

// ---------------------------------------------------------------------------
// OTA
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct OtaQuery {
    pub msg: Option<String>,
    pub error: Option<String>,
    /// 从设备面板「发起升级」跳转时预选的目标设备
    pub sn: Option<String>,
}

pub async fn ota_page(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<OtaQuery>,
) -> Result<Html<String>, AppError> {
    let firmware = state
        .store
        .firmware()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(firmware_view)
        .collect();
    let visible = visible_device_sns(&state, &user).await;
    let devices = state
        .store
        .devices()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|d| visible.contains(&d.sn))
        .map(|d| DeviceOption {
            sn: d.sn,
            mdvr_id: d.mdvr_id,
            online: false,
        })
        .collect::<Vec<_>>();
    let online = state.registry.list().await;
    let devices = devices
        .into_iter()
        .map(|mut d| {
            d.online = online.iter().any(|o| o.sn == d.sn);
            d
        })
        .collect();

    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let jobs = state
        .store
        .jobs(60)
        .await
        .unwrap_or_default()
        .iter()
        .filter(|j| visible.contains(&j.sn))
        .take(30)
        .map(|j| job_view(&t, j, state.ota.progress(&j.id)))
        .collect();
    let can_manage = state.rbac.enforce(&user.role, "/ota/upload", "POST");

    OtaTemplate {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        i18n_js: i18n::js_dict(&state.settings.lang()),
        user,
        active: "ota",
        firmware,
        devices,
        jobs,
        flash: q.msg.unwrap_or_default(),
        error: q.error.unwrap_or_default(),
        can_manage,
        selected_sn: q.sn.unwrap_or_default(),
    }
    .render()
    .map(Html)
    .map_err(|e| AppError(anyhow::anyhow!(e)))
}

fn firmware_view(f: FirmwareRow) -> FirmwareView {
    FirmwareView {
        id: f.id,
        filename: f.filename,
        version: f.version,
        size: fmt_size(f.size),
        sha256: f.sha256.chars().take(12).collect(),
        uploaded: fmt_ts(f.uploaded_at_ms),
        uploaded_by: f.uploaded_by,
    }
}

pub async fn ota_upload(
    State(state): State<AppState>,
    user: AuthUser,
    mut multipart: Multipart,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let mut version = String::new();
    let mut filename = String::new();
    let mut target_sn = String::new();
    let mut data: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name() {
            Some("version") => version = field.text().await.unwrap_or_default(),
            // 快速升级：带上目标 SN 时，上传完成后立即发起升级
            Some("sn") => target_sn = field.text().await.unwrap_or_default(),
            Some("file") => {
                filename = field.file_name().unwrap_or("firmware.bin").to_string();
                match field.bytes().await {
                    Ok(b) => data = Some(b.to_vec()),
                    Err(e) => {
                        return ToastTemplate {
                            ok: false,
                            message: format!("{}: {e}", t.tr("cmd.err_read_fw")),
                        }
                        .render()
                        .map(|h| Html(h).into_response())
                        .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
                        .into_response();
                    }
                }
            }
            _ => {}
        }
    }

    let Some(bytes) = data else {
        return ToastTemplate {
            ok: false,
            message: t.tr("cmd.err_no_firmware").into(),
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
        .into_response();
    };
    if bytes.is_empty() {
        return ToastTemplate {
            ok: false,
            message: t.tr("cmd.err_fw_empty").into(),
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
        .into_response();
    }
    if bytes.len() > 8 * 1024 * 1024 {
        return ToastTemplate {
            ok: false,
            message: t.tr("cmd.err_fw_size").into(),
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::PAYLOAD_TOO_LARGE.into_response())
        .into_response();
    }

    use sha2::{Digest, Sha256};
    let sha = hex::encode(Sha256::digest(&bytes));
    let id = uuid::Uuid::new_v4().to_string();
    let path = state.cfg.firmware_dir.join(format!("{id}.bin"));
    if let Err(e) = tokio::fs::create_dir_all(&state.cfg.firmware_dir).await {
        return ToastTemplate {
            ok: false,
            message: format!("{}: {e}", t.tr("cmd.err_mkdir")),
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        .into_response();
    }
    if let Err(e) = tokio::fs::write(&path, &bytes).await {
        return ToastTemplate {
            ok: false,
            message: format!("{}: {e}", t.tr("cmd.err_save_fw")),
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        .into_response();
    }

    let version = if version.trim().is_empty() {
        "0.0.0".to_string()
    } else {
        version.trim().to_string()
    };
    let fw_id = id.clone();
    state.store.insert_firmware(FirmwareRow {
        id,
        filename: filename.clone(),
        version: version.clone(),
        size: bytes.len() as i64,
        sha256: sha.clone(),
        uploaded_at_ms: Utc::now().timestamp_millis(),
        uploaded_by: user.username.clone(),
    });
    state.store.audit(
        &user.username,
        "ota.upload",
        &filename,
        &format!("version={version} size={} sha256={}", bytes.len(), &sha[..12]),
    );

    // 快速升级：表单带 sn 时上传后立即开始
    if !target_sn.trim().is_empty() {
        if !can_see_device(&state, &user, &target_sn).await {
            return toast_err(t.tr("cmd.err_dev_upgrade"));
        }
        return match state
            .ota
            .start(target_sn.trim(), &fw_id, &user.username)
            .await
        {
            Ok(job_id) => redirect_with_flash(
                "/ota",
                &t.trf(
                    "flash.ota_quick_started",
                    &[("id", job_id[..8.min(job_id.len())].to_string())],
                ),
            ),
            Err(e) => ToastTemplate {
                ok: false,
                message: format!("{}：{e}", t.tr("cmd.err_start")),
            }
            .render()
            .map(|h| Html(h).into_response())
            .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
            .into_response(),
        };
    }

    redirect_with_flash(
        "/ota",
        &t.trf("flash.fw_uploaded", &[("file", filename.clone()), ("version", version.clone())]),
    )
}

#[derive(Deserialize)]
pub struct OtaStartForm {
    pub sn: String,
    pub firmware_id: String,
}

pub async fn ota_start(
    State(state): State<AppState>,
    user: AuthUser,
    Form(form): Form<OtaStartForm>,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    if !can_see_device(&state, &user, &form.sn).await {
        return toast_err(t.tr("cmd.err_dev_upgrade"));
    }
    match state
        .ota
        .start(&form.sn, &form.firmware_id, &user.username)
        .await
    {
        Ok(job_id) => redirect_with_flash(
            "/ota",
            &t.trf("flash.ota_started", &[("id", job_id[..8.min(job_id.len())].to_string())]),
        ),
        Err(e) => ToastTemplate {
            ok: false,
            message: format!("{}：{e}", t.tr("cmd.err_start")),
        }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
        .into_response(),
    }
}

pub async fn ota_progress(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let job = state.store.job(&id).await.unwrap_or_default();
    let Some(job) = job else {
        return Ok(Html(format!("<div class=\"toast err\">{}</div>", t.tr("cmd.err_job"))));
    };
    if !can_see_device(&state, &user, &job.sn).await {
        return Ok(Html(format!("<div class=\"toast err\">{}</div>", t.tr("cmd.err_job"))));
    }
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let live = state.ota.progress(&id);
    let mut view = job_view(&t, &job, live);
    let fw_version = state
        .store
        .firmware_by_id(&job.firmware_id)
        .await
        .unwrap_or_default()
        .map(|f| f.version)
        .unwrap_or_default();
    view.firmware_version = fw_version.clone();
    ProgressTemplate {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        job: view,
    }
    .render()
    .map(Html)
    .map_err(|e| AppError(anyhow::anyhow!(e)))
}

pub async fn ota_job_json(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let job = state.store.job(&id).await?;
    let Some(job) = job else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    if !can_see_device(&state, &user, &job.sn).await {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let mut view = job_view(&t, &job, state.ota.progress(&id));
    view.firmware_version = state
        .store
        .firmware_by_id(&job.firmware_id)
        .await?
        .map(|f| f.version)
        .unwrap_or_default();
    Ok(axum::Json(serde_json::json!({
        "id": view.id,
        "sn": view.sn,
        "state": view.state,
        "percent": view.percent,
        "sent": view.sent,
        "total": view.total,
        "message": view.message,
        "firmware_version": view.firmware_version,
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// 用户管理
// ---------------------------------------------------------------------------

pub async fn users_page(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<FlashQuery>,
) -> Result<Html<String>, AppError> {
    let rows = state.store.users().await.unwrap_or_default();
    let devices = state.store.devices().await.unwrap_or_default();
    let root = root_admin(&rows);
    let mut depth_of: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for u in &rows {
        depth_of.insert(u.username.clone(), depth_in(&rows, &u.username));
    }
    // 按树的先序排列：根管理员 → 其下级，保证列表与层级一致
    let mut ordered: Vec<&crate::storage::UserRow> = Vec::new();
    fn push_children<'a>(
        rows: &'a [crate::storage::UserRow],
        parent: &str,
        out: &mut Vec<&'a crate::storage::UserRow>,
    ) {
        for u in rows.iter().filter(|u| u.parent == parent) {
            out.push(u);
            push_children(rows, &u.username, out);
        }
    }
    push_children(&rows, "", &mut ordered);
    for u in &rows {
        if !ordered.iter().any(|o| o.username == u.username) {
            ordered.push(u);
        }
    }
    let users = ordered
        .into_iter()
        .map(|u| {
            let blocked = tree::subtree_usernames(&rows, &u.username);
            let visible_devices = if u.username == root {
                devices.len()
            } else {
                devices
                    .iter()
                    .filter(|d| !d.owner.is_empty() && blocked.contains(&d.owner))
                    .count()
            };
            let mut parent_choices: Vec<ParentChoice> = rows
                .iter()
                .filter(|p| !blocked.contains(&p.username))
                .map(|p| ParentChoice {
                    name: p.username.clone(),
                    selected: p.username == u.parent,
                })
                .collect();
            // 根管理员排在最前，方便「挂到 admin 下」这个最常见的操作
            parent_choices.sort_by_key(|p| (p.name != root, p.name.clone()));
            UserView {
                is_self: u.username == user.username,
                username: u.username.clone(),
                role: u.role.clone(),
                created: fmt_ts(u.created_at_ms),
                parent: u.parent.clone(),
                depth: *depth_of.get(&u.username).unwrap_or(&0),
                devices: devices.iter().filter(|d| d.owner == u.username).count(),
                visible_devices,
                children: rows.iter().filter(|c| c.parent == u.username).count(),
                parent_choices,
            }
        })
        .collect();
    // 新建用户默认挂在根管理员下，其余上级按名称排序
    let mut parent_options: Vec<String> = rows
        .iter()
        .map(|u| u.username.clone())
        .filter(|n| n != &root)
        .collect();
    parent_options.sort();
    parent_options.insert(0, root.clone());
    UsersTemplate {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        i18n_js: i18n::js_dict(&state.settings.lang()),
        user,
        active: "users",
        root: root.clone(),
        users,
        parent_options,
        flash: q.msg.unwrap_or_default(),
        error: String::new(),
        subtitle: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")).trf("users.subtitle", &[("root", root.clone())]),
        scope_admin: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")).trf("users.scope_admin", &[("root", root.clone())]),
        hierarchy_body: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"))
            .trf("users.hierarchy_body", &[("root", root.clone())]),
        unassigned_body: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"))
            .trf("users.unassigned_body", &[("root", root.clone())]),
    }
    .render()
    .map(Html)
    .map_err(|e| AppError(anyhow::anyhow!(e)))
}

#[derive(Deserialize)]
pub struct CreateUserForm {
    pub username: String,
    pub password: String,
    pub role: String,
    /// 上级用户，留空时默认挂到根管理员下
    pub parent: String,
}

/// 重置密码表单：只改口令，不动角色与层级。
#[derive(Deserialize)]
pub struct ResetPasswordForm {
    pub username: String,
    pub password: String,
}

/// 用户名规则：3–32 位字母、数字与 `_ - .`。
///
/// 不只是美观——用户名会进树键（`u:{name}`）、CSS 类、CSS 选择器和审计详情，
/// 放行空格 / 引号 / 斜杠只会给后续排障添乱。
pub fn valid_username(name: &str) -> bool {
    let n = name.trim();
    (3..=32).contains(&n.chars().count())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// 口令规则：至少 6 位，且不能全是空白（否则等于没设口令）。
pub fn valid_password(pwd: &str) -> bool {
    pwd.chars().count() >= 6 && !pwd.trim().is_empty()
}

/// 层级以管理员为根：优先取名为 admin 的账号，其次取任意 admin 角色账号。
fn root_admin(users: &[crate::storage::UserRow]) -> String {
    users
        .iter()
        .find(|u| u.username == "admin")
        .or_else(|| users.iter().find(|u| u.role == "admin"))
        .map(|u| u.username.clone())
        .unwrap_or_else(|| "admin".to_string())
}

pub async fn user_create(
    State(state): State<AppState>,
    user: AuthUser,
    Form(form): Form<CreateUserForm>,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let username = form.username.trim();
    if !valid_username(username) {
        return toast_err(if username.len() < 3 {
            t.tr("cmd.err_user_len")
        } else {
            t.tr("cmd.err_user_format")
        });
    }
    if !valid_password(&form.password) {
        return toast_err(t.tr("cmd.err_pwd_len"));
    }
    let allowed = ["viewer", "operator", "firmware_manager", "admin"];
    if !allowed.contains(&form.role.as_str()) {
        return toast_err(t.tr("cmd.err_role"));
    }
    let users = state.store.users().await.unwrap_or_default();
    // 「创建」只负责新建：重名一律拒绝，避免误把已有账号的口令/角色覆盖掉。
    // 要改口令走 /admin/users/password，要改角色先删再建。
    if users.iter().any(|u| u.username == username) {
        return toast_err(t.tr("cmd.err_user_exists"));
    }
    let root = root_admin(&users);
    // 所有用户都在管理员之下：留空即挂到根管理员
    let parent = match form.parent.trim() {
        "" => root.clone(),
        p => p.to_string(),
    };
    if parent == username {
        return toast_err(t.tr("cmd.err_parent_self"));
    }
    if !users.iter().any(|u| u.username == parent) {
        return toast_err(t.tr("cmd.err_parent_missing"));
    }
    match auth::hash_password(&form.password) {
        Ok(hash) => {
            state.store.upsert_user(username, &hash, &form.role, Some(&parent));
            state.store.audit(
                &user.username,
                "user.create",
                username,
                &format!("role={} parent={}", form.role, parent),
            );
            redirect_with_flash(
                "/admin/users",
                &t.trf("flash.user_created", &[("name", username.to_string())]),
            )
        }
        Err(e) => toast_err(&e.to_string()),
    }
}

/// 重置密码：唯一能改已有账号口令的入口。角色与层级保持不变。
pub async fn user_password(
    State(state): State<AppState>,
    user: AuthUser,
    Form(form): Form<ResetPasswordForm>,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let username = form.username.trim();
    if !valid_password(&form.password) {
        return toast_err(t.tr("cmd.err_pwd_len"));
    }
    let users = state.store.users().await.unwrap_or_default();
    let Some(target) = users.iter().find(|u| u.username == username) else {
        return toast_err(t.tr("cmd.err_user_missing"));
    };
    let hash = match auth::hash_password(&form.password) {
        Ok(h) => h,
        Err(e) => return toast_err(&e.to_string()),
    };
    // parent 传 None：upsert 的 ON CONFLICT 分支在 parent 为空时保留原层级
    state
        .store
        .upsert_user(username, &hash, &target.role, None);
    state
        .store
        .audit(&user.username, "user.password", username, "重置密码");
    redirect_with_flash(
        "/admin/users",
        &t.trf("flash.password_reset", &[("name", username.to_string())]),
    )
}

#[derive(Deserialize)]
pub struct DeleteUserForm {
    pub username: String,
}

/// 删除用户前的全部守卫。返回 `Err(i18n 键)` 表示拒绝及原因。
///
/// 单独抽成纯函数是为了能直接测：删自己、删根、删最后一个管理员、
/// 有下级、名下有设备，五条互不影响。
fn check_user_delete(
    target: &crate::storage::UserRow,
    actor: &str,
    users: &[crate::storage::UserRow],
    has_devices: bool,
) -> Result<(), &'static str> {
    if target.username == actor {
        return Err("cmd.err_delete_self");
    }
    if target.username == root_admin(users) {
        return Err("cmd.err_root_delete");
    }
    // 层级本身保证所有人都挂在根管理员下，「根有下级」通常已拦住删根；
    // 这里再兜一层：最后一个管理员不能删，否则 /admin/* 谁都进不去。
    let admins = users.iter().filter(|u| u.role == "admin").count();
    if target.role == "admin" && admins <= 1 {
        return Err("cmd.err_last_admin");
    }
    if users.iter().any(|u| u.parent == target.username) {
        return Err("cmd.err_user_children");
    }
    if has_devices {
        return Err("cmd.err_user_devices");
    }
    Ok(())
}

pub async fn user_delete(
    State(state): State<AppState>,
    user: AuthUser,
    Form(form): Form<DeleteUserForm>,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let users = state.store.users().await.unwrap_or_default();
    let Some(target) = users.iter().find(|u| u.username == form.username) else {
        return toast_err(t.tr("cmd.err_user_missing"));
    };
    let devices = state.store.devices().await.unwrap_or_default();
    let has_devices = devices.iter().any(|d| d.owner == form.username);
    if let Err(key) = check_user_delete(target, &user.username, &users, has_devices) {
        return toast_err(t.tr(key));
    }
    state.store.delete_user(&form.username);
    state
        .store
        .audit(&user.username, "user.delete", &form.username, "");
    redirect_with_flash("/admin/users", &t.trf("flash.user_deleted", &[("name", form.username.clone())]))
}

#[derive(Deserialize)]
pub struct UserParentForm {
    pub username: String,
    pub parent: String,
}

/// 调整用户的上级。禁止把自己挂到自己的子树下，否则会形成环。
/// 层级以管理员为根：上级留空等同于挂到根管理员，根管理员本身不可被下移。
pub async fn user_parent(
    State(state): State<AppState>,
    user: AuthUser,
    Form(form): Form<UserParentForm>,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let users = state.store.users().await.unwrap_or_default();
    let root = root_admin(&users);
    if !users.iter().any(|u| u.username == form.username) {
        return toast_err(t.tr("cmd.err_user_missing"));
    }
    let parent = match form.parent.trim() {
        "" => root.clone(),
        p => p.to_string(),
    };
    if parent == form.username {
        return toast_err(t.tr("cmd.err_parent_self"));
    }
    if form.username == root {
        return toast_err(t.tr("cmd.err_root_top"));
    }
    if form.username == user.username {
        return toast_err(t.tr("cmd.err_self_parent"));
    }
    if !users.iter().any(|u| u.username == parent) {
        return toast_err(t.tr("cmd.err_parent_missing"));
    }
    if tree::subtree_usernames(&users, &form.username).contains(&parent) {
        return toast_err(t.tr("cmd.err_subtree"));
    }
    state.store.set_user_parent(&form.username, &parent);
    state.store.audit(
        &user.username,
        "user.parent",
        &form.username,
        &format!("parent={parent}"),
    );
    redirect_with_flash(
        "/admin/users",
        &t.trf("flash.parent_updated", &[("name", form.username.clone()), ("parent", parent.clone())]),
    )
}

/// 计算用户深度，遇到断链或环按已走层级返回（上限 32，防止脏数据把缩进撑爆）。
fn depth_in(users: &[crate::storage::UserRow], name: &str) -> usize {
    let mut depth = 0;
    let mut cur = name.to_string();
    let mut guard = 0;
    while let Some(u) = users.iter().find(|u| u.username == cur) {
        if u.parent.is_empty() || u.parent == u.username || guard >= 32 {
            break;
        }
        cur = u.parent.clone();
        depth += 1;
        guard += 1;
    }
    depth
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FlashQuery {
    pub msg: Option<String>,
    pub error: Option<String>,
}

fn redirect_with_flash(path: &str, msg: &str) -> Response {
    let encoded: String = urlencode(msg);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        format!("{path}?msg={encoded}").parse().unwrap(),
    );
    (StatusCode::SEE_OTHER, headers).into_response()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn sanitize_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") => n.to_string(),
        _ => "/analysis".into(),
    }
}

pub async fn forbidden_page(user: AuthUser, path: String, method: String) -> Response {
    let t = Tr::from(i18n::DEFAULT_LANG);
    let body = t.trf(
        "forbidden.body",
        &[
            ("role", user.role.clone()),
            ("method", method.clone()),
            ("path", path.clone()),
        ],
    );
    ForbiddenTemplate {
        t,
        i18n_js: i18n::js_dict(i18n::DEFAULT_LANG),
        user,
        active: "",
        path,
        method,
        body,
    }
    .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::FORBIDDEN.into_response())
        .into_response()
}

pub fn forbidden_response(user: &AuthUser, path: &str, method: &str) -> Response {
    let t = Tr::from(i18n::DEFAULT_LANG);
    let mut resp = ForbiddenTemplate {
        t,
        i18n_js: i18n::js_dict(i18n::DEFAULT_LANG),
        user: user.clone(),
        active: "",
        path: path.to_string(),
        method: method.to_string(),
        body: Tr::from(i18n::DEFAULT_LANG).trf(
            "forbidden.body",
            &[
                ("role", user.role.clone()),
                ("method", method.to_string()),
                ("path", path.to_string()),
            ],
        ),
    }
    .render()
    .map(|h| Html(h).into_response())
    .unwrap_or_else(|_| StatusCode::FORBIDDEN.into_response())
    .into_response();
    *resp.status_mut() = StatusCode::FORBIDDEN;
    resp
}

// ---------------------------------------------------------------------------
// 设备树 / 实时监控
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TreeStats {
    pub total: usize,
    pub online: usize,
    pub abnormal: usize,
    pub users: usize,
    pub groups: usize,
}

fn tree_stats(forest: &[UserNode]) -> TreeStats {
    fn walk(n: &UserNode, s: &mut TreeStats) {
        s.total += n.groups.iter().map(|g| g.total).sum::<usize>();
        s.online += n.groups.iter().map(|g| g.online).sum::<usize>();
        s.groups += n.groups.len();
        s.users += 1;
        for g in &n.groups {
            s.abnormal += g.devices.iter().filter(|d| d.health != 0).count();
        }
        for c in &n.children {
            walk(c, s);
        }
    }
    let mut s = TreeStats::default();
    for n in forest {
        walk(n, &mut s);
    }
    s
}

/// 一次性取齐建树所需的数据（用户、设备、最新状态、在线表）。
async fn load_forest(state: &AppState, user: &AuthUser) -> Vec<UserNode> {
    let users = state.store.users().await.unwrap_or_default();
    let devices = state.store.devices().await.unwrap_or_default();
    let latest: HashMap<String, crate::storage::LatestRow> = state
        .store
        .latest_all()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|l| (l.sn.clone(), l))
        .collect();
    let live = state.registry.list().await;
    let online: HashMap<String, bool> = live.iter().map(|l| (l.sn.clone(), true)).collect();
    let busy: HashSet<String> = live
        .iter()
        .filter(|l| l.busy)
        .map(|l| l.sn.clone())
        .collect();
    tree::build_tree(
        &users,
        &devices,
        &latest,
        &online,
        &busy,
        &user.username,
        user.is_admin(),
    )
}

/// 设备级越权校验：非管理员只能看自己子树的设备。
/// 走设备点查而不是拉全表——这个函数在面板轮询等 9 个入口上都会被调用。
async fn can_see_device(state: &AppState, user: &AuthUser, sn: &str) -> bool {
    if user.is_admin() {
        return true;
    }
    let Some(d) = state.store.device(sn).await.ok().flatten() else {
        return false;
    };
    if d.owner.is_empty() {
        return false;
    }
    let users = state.store.users().await.unwrap_or_default();
    tree::subtree_usernames(&users, &user.username).contains(&d.owner)
}

/// 当前用户可见的设备 SN 集合：管理员为全部（含未归属），其余为「自己 + 全部下级」。
/// 列表页（总览 / OTA）统一用它过滤，避免只靠模板显隐造成越权。
async fn visible_device_sns(state: &AppState, user: &AuthUser) -> HashSet<String> {
    let devices = state.store.devices().await.unwrap_or_default();
    if user.is_admin() {
        return devices.into_iter().map(|d| d.sn).collect();
    }
    let users = state.store.users().await.unwrap_or_default();
    let scope = tree::subtree_usernames(&users, &user.username);
    devices
        .into_iter()
        .filter(|d| !d.owner.is_empty() && scope.contains(&d.owner))
        .map(|d| d.sn)
        .collect()
}

#[derive(Template)]
#[template(path = "monitor.html")]
struct MonitorTemplate {
    t: Tr,
    i18n_js: String,
    user: AuthUser,
    active: &'static str,
    tree_json: String,
    stats: TreeStats,
    initial: String,
    can_assign: bool,
}

#[derive(Deserialize)]
pub struct MonitorQuery {
    pub sn: Option<String>,
}

pub async fn monitor_page(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<MonitorQuery>,
) -> Result<Html<String>, AppError> {
    let forest = load_forest(&state, &user).await;
    let stats = tree_stats(&forest);
    let json = serde_json::to_string(&forest)?;
    let can_assign = state.rbac.enforce(&user.role, "/devices/*/assign", "POST");
    let initial = q.sn.unwrap_or_default();
    // 用户名里可能出现 </script>，把 < 转义掉，保证内联 JSON 安全
    let tree_json = json.replace('<', "\\u003c");
    MonitorTemplate {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        i18n_js: i18n::js_dict(&state.settings.lang()),
        user,
        active: "monitor",
        tree_json,
        stats,
        initial,
        can_assign,
    }
    .render()
    .map(Html)
    .map_err(|e| AppError(anyhow::anyhow!(e)))
}

pub async fn tree_json(State(state): State<AppState>, user: AuthUser) -> Response {
    let forest = load_forest(&state, &user).await;
    let stats = tree_stats(&forest);
    // idle_secs 让前端锁定倒计时与「系统设置」里的值保持一致，避免两边写死不同数字
    let idle_secs = state
        .settings
        .i64("session_idle_min", state.cfg.session_idle_min)
        .clamp(1, 720)
        * 60;
    axum::Json(serde_json::json!({
        "tree": forest,
        "stats": stats,
        "idle_secs": idle_secs,
    }))
    .into_response()
}

/// 右侧面板里的实时指标块，独立轮询，避免整块重绘把表单输入清空。
pub async fn device_live(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sn): Path<String>,
) -> Result<Html<String>, AppError> {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    if !can_see_device(&state, &user, &sn).await {
        return Ok(Html(format!("<div class='empty-hint'>{}</div>", t.tr("cmd.err_device"))));
    }
    let live = state.registry.get(&sn).await;
    let latest = state.store.latest(&sn).await.ok().flatten();
    let row = state.store.device(&sn).await.ok().flatten().unwrap_or_default();
    DeviceLive {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        i18n_js: i18n::js_dict(&state.settings.lang()),
        sn,
        online: live.is_some(),
        busy: live.as_ref().map(|l| l.is_busy()).unwrap_or(false),
        battery: latest.as_ref().map(|s| s.battery).unwrap_or(0),
        health: latest
            .as_ref()
            .map(|s| health_label(&t, s.health).to_string())
            .unwrap_or_else(|| t.tr("common.unknown").into()),
        health_class: latest
            .as_ref()
            .map(|s| health_class(s.health).to_string())
            .unwrap_or_else(|| "warn".into()),
        input_voltage: latest
            .as_ref()
            .map(|s| format!("{:.1}", s.input_voltage))
            .unwrap_or_else(|| "—".into()),
        output_voltage: latest
            .as_ref()
            .map(|s| format!("{:.1}", s.output_voltage))
            .unwrap_or_else(|| "—".into()),
        temperature: latest
            .as_ref()
            .map(|s| format!("{:.1}", s.temperature))
            .unwrap_or_else(|| "—".into()),
        firmware: latest
            .as_ref()
            .map(|s| s.firmware.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or(row.firmware_version),
        last_seen: fmt_relative(
            &t,
            live.as_ref()
                .map(|l| l.last_seen_ms())
                .unwrap_or(row.last_seen_ms),
        ),
        uptime: live
            .as_ref()
            .map(|l| format!("{} {}", l.uptime_secs() / 60, t.tr("common.minutes")))
            .unwrap_or_else(|| "—".into()),
        remote: live
            .as_ref()
            .map(|l| l.remote.to_string())
            .unwrap_or_else(|| "—".into()),
    }
    .render()
    .map(Html)
    .map_err(|e| AppError(anyhow::anyhow!(e)))
}

#[derive(Template)]
#[template(path = "device_live.html")]
#[allow(dead_code)]
struct DeviceLive {
    t: Tr,
    i18n_js: String,
    sn: String,
    online: bool,
    busy: bool,
    battery: i64,
    health: String,
    health_class: String,
    input_voltage: String,
    output_voltage: String,
    temperature: String,
    firmware: String,
    last_seen: String,
    uptime: String,
    remote: String,
}

#[derive(Deserialize)]
pub struct AssignForm {
    pub owner: String,
    pub group: String,
}

pub async fn device_assign(
    State(state): State<AppState>,
    user: AuthUser,
    Path(sn): Path<String>,
    Form(form): Form<AssignForm>,
) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    let owner = form.owner.trim().to_string();
    let group = form.group.trim().to_string();
    if !owner.is_empty() {
        let users = state.store.users().await.unwrap_or_default();
        if !users.iter().any(|u| u.username == owner) {
            return toast_err(t.tr("cmd.err_owner_missing"));
        }
    }
    // 设备不存在时 UPDATE 会影响 0 行，但界面照样提示"已更新"——先点查一遍
    if state.store.device(&sn).await.ok().flatten().is_none() {
        return toast_err(t.tr("cmd.err_device"));
    }
    state.store.assign_device(&sn, &owner, &group);
    state.store.audit(
        &user.username,
        "device.assign",
        &sn,
        &format!(
            "owner={}",
            if owner.is_empty() {
                t.tr("common.unassigned")
            } else {
                &owner
            }
        ),
    );
    let message = if owner.is_empty() {
        t.trf("flash.assign_cleared", &[("sn", sn.clone())])
    } else {
        t.trf(
            "flash.assign_updated",
            &[(
                "owner",
                owner.clone(),
            ), (
                "group",
                if group.is_empty() {
                    t.tr("common.default_group").to_string()
                } else {
                    group.clone()
                },
            )],
        )
    };
    let mut resp = ToastTemplate { ok: true, message }
        .render()
        .map(|h| Html(h).into_response())
        .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
        .into_response();
    // 通知前端重拉设备树
    resp.headers_mut()
        .insert("HX-Trigger", "tree-refresh".parse().unwrap());
    resp
}

fn toast_err(msg: &str) -> Response {
    ToastTemplate {
        ok: false,
        message: msg.to_string(),
    }
    .render()
    .map(|h| Html(h).into_response())
    .unwrap_or_else(|_| StatusCode::BAD_REQUEST.into_response())
    .into_response()
}

// ---------------------------------------------------------------------------
// 系统设置（/admin/settings）
// ---------------------------------------------------------------------------

/// 手工 HTML 转义（控件值以 |safe 注入，必须先转义）。
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

struct SettingRowView {
    label: String,
    hint: String,
    /// 预渲染的控件 HTML（服务端转义后注入）
    control: String,
    restart: bool,
}

struct SettingsGroupView {
    name: String,
    settings: Vec<SettingRowView>,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    t: Tr,
    i18n_js: String,
    user: AuthUser,
    active: &'static str,
    groups: Vec<SettingsGroupView>,
    flash: String,
    error: String,
}

fn setting_control(def: &crate::settings::SettingDef, value: &str, lang: &str) -> String {
    use crate::settings::Kind;
    let v = esc(value);
    match def.kind {
        Kind::Addr => format!(
            r#"<input class="mono" name="{k}" value="{v}" placeholder="ip:port" required>"#,
            k = def.key
        ),
        Kind::U64 { min, max } => format!(
            r#"<input class="mono" type="number" name="{k}" value="{v}" min="{min}" max="{max}" required>"#,
            k = def.key
        ),
        Kind::Text { max_len } => format!(
            r#"<input name="{k}" value="{v}" maxlength="{max_len}">"#,
            k = def.key
        ),
        Kind::Bool => {
            let checked = if value == "true" { " checked" } else { "" };
            format!(
                r#"<label class="switch"><input type="checkbox" name="{k}" value="true"{checked}><span></span></label>"#,
                k = def.key
            )
        }
        Kind::Lang => {
            let mut out = String::from(r#"<select name="{k}">"#);
            out = out.replace("{k}", def.key);
            for (code, _) in crate::settings::SUPPORTED_LANGS {
                let sel = if *code == value || (*code == "zh-CN" && value.is_empty()) {
                    " selected"
                } else {
                    ""
                };
                let name = super::i18n::Tr::new(lang).tr(match *code {
                    "en-US" => "common.lang_en",
                    _ => "common.lang_zh",
                });
                out.push_str(&format!(
                    r#"<option value="{code}"{sel}>{name}</option>"#
                ));
            }
            out.push_str("</select>");
            out
        }
    }
}

fn settings_groups(state: &AppState) -> Vec<SettingsGroupView> {
    let lang = state.settings.lang();
    let t = super::i18n::Tr::new(lang.clone()).with_site(state.settings.str("site_title"));
    let snapshot = state.settings.snapshot();
    crate::settings::GROUPS
        .iter()
        .map(|(gkey, _)| {
            let rows: Vec<SettingRowView> = snapshot
                .iter()
                .filter(|(d, _)| d.group == *gkey)
                .map(|(d, v)| SettingRowView {
                    label: t.tr(d.label_key).to_string(),
                    hint: t.tr(d.hint_key).to_string(),
                    control: setting_control(d, v, &lang),
                    restart: d.restart,
                })
                .collect();
            SettingsGroupView {
                name: t.tr(&format!("settings.group.{gkey}")).to_string(),
                settings: rows,
            }
        })
        .collect()
}

async fn render_settings(state: &AppState, user: AuthUser, flash: String, error: String) -> Response {
    SettingsTemplate {
        t: Tr::from(state.settings.lang()).with_site(state.settings.str("site_title")),
        i18n_js: i18n::js_dict(&state.settings.lang()),
        user,
        active: "settings",
        groups: settings_groups(state),
        flash,
        error,
    }
    .render()
    .map(|h| Html(h).into_response())
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn settings_page(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<FlashQuery>,
) -> Response {
    render_settings(&state, user, q.msg.unwrap_or_default(), q.error.unwrap_or_default()).await
}

pub async fn settings_save(
    State(state): State<AppState>,
    user: AuthUser,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let before: HashMap<&'static str, String> = state
        .settings
        .snapshot()
        .into_iter()
        .map(|(d, v)| (d.key, v))
        .collect();
    let pairs: Vec<(String, String)> = crate::settings::CATALOG
        .iter()
        .map(|d| {
            // 复选框未勾选时表单里没有该键，按 false 处理
            let raw = match d.kind {
                crate::settings::Kind::Bool => {
                    form.get(d.key).cloned().unwrap_or_else(|| "false".into())
                }
                _ => form.get(d.key).cloned().unwrap_or_default(),
            };
            (d.key.to_string(), raw)
        })
        .collect();
    match state.settings.save(&state.store, &pairs).await {
        Ok(()) => {
            let restart_changed = crate::settings::CATALOG
                .iter()
                .filter(|d| d.restart)
                .any(|d| before.get(d.key) != Some(&form.get(d.key).cloned().unwrap_or_default()));
            // 语言可能刚被改掉，用保存后的语言渲染提示
            let t = super::i18n::Tr::new(state.settings.lang()).with_site(state.settings.str("site_title"));
            redirect_with_flash("/admin/settings", t.tr(msg_key(restart_changed)))
        }
        Err(e) => {
            let lang = state.settings.lang();
            render_settings(&state, user, String::new(), e.message(&lang)).await
        }
    }
}

fn msg_key(restart: bool) -> &'static str {
    if restart {
        "settings.saved_restart"
    } else {
        "settings.saved"
    }
}

/// 拉起一个用于重启的新实例。
/// Windows 下优先尝试脱离父进程所在的 Job Object（部分进程管理器会随父退出清理子进程），
/// 若 Job 不允许 breakaway 则回退到普通创建；两种方式都放入新进程组，避免 Ctrl+C 连带退出。
fn spawn_restarted_process(exe: &std::path::Path) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(exe);
    cmd.env("MDVR_UPS_RESTART_DELAY_MS", "1200")
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = cmd.spawn() {
            return Ok(child);
        }
        // Job 未开启 breakaway 权限时回退（否则正常场景下会误报重启失败）
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
}

/// 重启系统：拉起一个新的本进程实例（带启动延迟，等旧进程释放端口），随后退出当前进程。
/// 仅管理员可调用（RBAC 已限制 `/admin/*`，这里再校验一次角色）。
pub async fn admin_restart(State(state): State<AppState>, user: AuthUser) -> Response {
    let t = Tr::from(state.settings.lang()).with_site(state.settings.str("site_title"));
    if user.role != "admin" {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            let msg = t.trf("settings.restart_fail", &[("msg", e.to_string())]);
            return json_message(false, &msg);
        }
    };

    // 子进程延迟 1.2s 启动：旧进程约 0.4s 后退出，端口与 DuckDB 文件锁均已释放。
    // stdout/stderr 继承父进程（通常已重定向到日志文件），重启后日志可继续写入。
    match spawn_restarted_process(&exe) {
        Ok(_child) => {
            // 先让当前响应写回浏览器，再退出本进程
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                std::process::exit(0);
            });
            (
                [(header::CONTENT_TYPE, "application/json")],
                axum::Json(serde_json::json!({ "ok": true, "message": t.tr("settings.restarting") })),
            )
                .into_response()
        }
        Err(e) => {
            let msg = t.trf("settings.restart_fail", &[("msg", e.to_string())]);
            json_message(false, &msg)
        }
    }
}

#[cfg(test)]
mod analysis_tests {
    use super::*;

    #[test]
    fn metric_stats_min_avg_max() {
        let v = [10.0, 10.4, 14.6, 13.0];
        let (min, avg, max) = metric_stats(&v);
        assert_eq!(min, 10.0);
        assert_eq!(max, 14.6);
        assert!((avg - 12.0).abs() < 1e-9);
        assert_eq!(metric_stats(&[]), (0.0, 0.0, 0.0));
    }

    #[test]
    fn decimate_keeps_first_point_per_stride() {
        // 1000 个点最多画 400 个 → 步长 3 → 334 个下标
        let idx = decimate_indices(1000, 400);
        assert_eq!(idx.first(), Some(&0));
        assert_eq!(idx.len(), 334);
        assert!(idx.windows(2).all(|w| w[1] - w[0] == 3));
        // 少于上限时原样保留
        assert_eq!(decimate_indices(10, 400), (0..10).collect::<Vec<_>>());
        assert!(decimate_indices(0, 400).is_empty());
    }

    #[test]
    fn parse_date_accepts_iso_and_rejects_junk() {
        assert_eq!(
            parse_date(&Some("2026-10-20".into())),
            chrono::NaiveDate::from_ymd_opt(2026, 10, 20)
        );
        assert_eq!(parse_date(&Some("abc".into())), None);
        assert_eq!(parse_date(&None), None);
    }

    /// 用一条「平 → 充电 → 充满 → 平 → 放电」的电量曲线验证关键事件识别。
    /// 各阶段以 5 分钟间隔、且彼此间隔 > 15 分钟，避免被去重合并。
    #[test]
    fn detect_battery_events_finds_charge_and_full() {
        use chart::EventKind;
        // 每点间隔 5 分钟；阶段之间拉开时间，便于断言事件时间次序
        let mk = |vals: &[f64]| -> Vec<(i64, f64)> {
            vals.iter()
                .enumerate()
                .map(|(i, v)| (i as i64 * 5 * 60_000, *v))
                .collect()
        };
        // 50% 平(4点) → 升到 100%(12点) → 满电平(4点) → 降到 60%(10点)
        let series = mk(&[
            50.0, 50.0, 50.0, 50.0, // 0-15min 平
            55.0, 60.0, 66.0, 72.0, 79.0, 85.0, 90.0, 94.0, 97.0, 99.0, 100.0, 100.0, // 充电 20-75min
            100.0, 100.0, 100.0, 100.0, // 满电平 80-95min
            95.0, 88.0, 82.0, 76.0, 70.0, 66.0, 63.0, 61.0, 60.0, 60.0, // 放电 100-145min
        ]);
        let evs = detect_battery_events(&series);
        let kinds: Vec<EventKind> = evs.iter().map(|(_, k)| *k).collect();
        assert!(kinds.contains(&EventKind::Charge), "应识别充电开始: {kinds:?}");
        assert!(kinds.contains(&EventKind::Full), "应识别充满: {kinds:?}");
        assert!(
            kinds.contains(&EventKind::Discharge),
            "应识别放电开始: {kinds:?}"
        );
        // 事件时间次序：充电 < 充满 < 放电
        let charge_t = evs.iter().find(|(_, k)| *k == EventKind::Charge).map(|(t, _)| *t).unwrap();
        let full_t = evs.iter().find(|(_, k)| *k == EventKind::Full).map(|(t, _)| *t).unwrap();
        let disch_t = evs.iter().find(|(_, k)| *k == EventKind::Discharge).map(|(t, _)| *t).unwrap();
        assert!(charge_t < full_t, "充电应早于充满");
        assert!(full_t < disch_t, "充满应早于放电");
        // 无数据或极短序列返回空
        assert!(detect_battery_events(&[]).is_empty());
        assert!(detect_battery_events(&[(0, 50.0), (300_000, 50.0)]).is_empty());
    }

    /// 监控页底部固定展示最近 24 小时，不再接受自定义区间。
    #[test]
    fn recent_window_covers_last_24_hours() {
        let now = 1_700_000_000_000;
        let (from, to) = recent_window(now);
        assert_eq!(to, now);
        assert_eq!(to - from, 24 * 3600 * 1000);
    }

    // ---------------- 用户管理 ----------------

    fn user(name: &str, role: &str, parent: &str) -> crate::storage::UserRow {
        crate::storage::UserRow {
            username: name.into(),
            role: role.into(),
            created_at_ms: 0,
            parent: parent.into(),
        }
    }

    #[test]
    fn username_rules() {
        assert!(valid_username("abc"));
        assert!(valid_username("fleet-a.1_2"));
        assert!(!valid_username("ab"), "至少 3 位");
        assert!(!valid_username(""), "空");
        assert!(!valid_username("   "), "纯空白");
        assert!(!valid_username("a b c"), "空格不行");
        assert!(!valid_username("alice/admin"), "斜杠会破坏路径语义");
        assert!(!valid_username("alice&x=1"), "查询串分隔符");
        assert!(!valid_username("a".repeat(33).as_str()), "超过 32 位");
        assert!(valid_username(&"a".repeat(32)));
    }

    #[test]
    fn password_rules() {
        assert!(valid_password("123456"));
        assert!(valid_password("ab cd ef"));
        assert!(!valid_password("12345"), "至少 6 位");
        assert!(!valid_password("      "), "全空白等于没设");
        assert!(!valid_password(""), "空");
    }

    /// 深度计算必须对「自指」和「成环」刹车，否则缩进会变成 33 级。
    #[test]
    fn depth_stops_on_cycles() {
        let clean = vec![user("admin", "admin", ""), user("a", "viewer", "admin")];
        assert_eq!(depth_in(&clean, "a"), 1);
        let selfref = vec![user("admin", "admin", ""), user("x", "viewer", "x")];
        assert_eq!(depth_in(&selfref, "x"), 0);
        let ring = vec![
            user("admin", "admin", ""),
            user("a", "viewer", "b"),
            user("b", "viewer", "a"),
        ];
        assert!(depth_in(&ring, "a") <= 32, "环上深度必须有上限");
    }

    /// 根管理员优先按用户名 `admin` 找，其次按角色。
    #[test]
    fn root_admin_prefers_named_admin() {
        let rows = vec![user("boss", "admin", ""), user("admin", "viewer", "boss")];
        assert_eq!(root_admin(&rows), "admin");
        let rows = vec![user("boss", "admin", "")];
        assert_eq!(root_admin(&rows), "boss");
        assert_eq!(root_admin(&[]), "admin");
    }

    /// 删除守卫：五条规则各自独立生效。
    #[test]
    fn delete_guards_fire_independently() {
        let rows = vec![
            user("admin", "admin", ""),
            user("boss", "admin", "admin"),
            user("alice", "viewer", "admin"),
            user("bob", "viewer", "alice"),
        ];
        let find = |n: &str| rows.iter().find(|u| u.username == n).unwrap();

        assert_eq!(check_user_delete(find("admin"), "root2", &rows, false), Err("cmd.err_root_delete"));
        assert_eq!(check_user_delete(find("alice"), "alice", &rows, false), Err("cmd.err_delete_self"));
        // 两个管理员时删掉其中一个没问题
        assert_eq!(check_user_delete(find("boss"), "admin", &rows, false), Ok(()));
        // 只剩一个管理员时不能删（哪怕他不是根）
        let solo = vec![user("admin", "viewer", ""), user("boss", "admin", "admin")];
        assert_eq!(
            check_user_delete(solo.iter().find(|u| u.username == "boss").unwrap(), "admin", &solo, false),
            Err("cmd.err_last_admin")
        );
        // 有下级 / 名下有设备
        assert_eq!(check_user_delete(find("alice"), "admin", &rows, false), Err("cmd.err_user_children"));
        assert_eq!(check_user_delete(find("bob"), "admin", &rows, false), Ok(()));
        assert_eq!(check_user_delete(find("bob"), "admin", &rows, true), Err("cmd.err_user_devices"));
    }
}
