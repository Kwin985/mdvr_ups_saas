//! JWT 认证与口令哈希。

use anyhow::{Result, anyhow};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::response::IntoResponse;
use chrono::{Duration, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

use crate::config::Config;

pub const COOKIE_NAME: &str = "ups_token";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub role: String,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub role: String,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }

    pub fn initial(&self) -> String {
        self.username.chars().next().unwrap_or('U').to_uppercase().to_string()
    }
}

/// 签发令牌。`ttl_secs` 即会话空闲窗口——中间件会在剩余不足一半时滑动续期。
pub fn create_token(
    cfg: &Config,
    ttl_secs: i64,
    username: &str,
    role: &str,
) -> Result<String> {
    let now = Utc::now();
    let claims = Claims {
        sub: username.to_string(),
        role: role.to_string(),
        iat: now.timestamp(),
        exp: (now + Duration::seconds(ttl_secs)).timestamp(),
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(cfg.jwt_secret.as_bytes()),
    )
    .map_err(|e| anyhow!("签发 token 失败: {e}"))
}

pub fn verify_token(cfg: &Config, token: &str) -> Option<Claims> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(cfg.jwt_secret.as_bytes()),
        &validation,
    )
    .ok()
    .map(|d| d.claims)
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("口令哈希失败: {e}"))
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// 从 Cookie 头中取出 token。
pub fn token_from_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return Some(v.to_string());
        }
    }
    None
}

pub fn set_auth_cookie(token: &str, max_age_secs: i64) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}"
    )
}

pub fn clear_auth_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// 单个 key 的失败记录：连续失败次数、锁定截止时刻、最后一次失败时刻。
/// `last_fail` 只服务于清理——零星失败在一个锁定时长后失去意义，该删就删。
#[derive(Debug, Clone, Copy)]
struct FailRecord {
    fails: u32,
    locked_until: Option<std::time::Instant>,
    last_fail: std::time::Instant,
}

type FailMap = std::sync::Mutex<std::collections::HashMap<String, FailRecord>>;

/// 登录防爆破：同一来源连续输错达到阈值后按分钟级锁定。
///
/// 为什么放在进程内而不是数据库：失败的登录还没有身份，写库反而给
/// 未认证流量开了写路径；进程内存表重启即清空，正好匹配「重启=重新开始」。
/// 用 `Mutex<HashMap>` 而不是 `DashMap`：临界区只有几次哈希查找，且要
/// 保证「检查 + 计数」对一个 key 是原子的，避免并发请求同时穿过阈值。
#[derive(Debug, Default)]
pub struct LoginThrottle {
    /// key（来源 IP）-> 失败记录
    fails: FailMap,
}

/// 连续失败多少次后开始锁定。
pub const THROTTLE_MAX_FAILS: u32 = 5;
/// 锁定时长（分钟）。锁定是整段的，不随继续尝试延长——否则一次误锁会越试越长。
pub const THROTTLE_LOCK_MIN: u64 = 15;

impl LoginThrottle {
    /// 是否放行。放行时才应该继续走口令校验；返回 false 直接拒绝。
    ///
    /// 注意这里**不能**顺手清零失败计数：每次登录尝试都会先调 allow，
    /// 一清零计数就永远攒不到阈值（防爆破形同虚设）。计数只在
    /// `record_failure` 里递增、在 `record_success` / 锁定过期时归零。
    pub fn allow(&self, key: &str) -> bool {
        let now = std::time::Instant::now();
        let map = self.fails.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key) {
            // 只有「锁定且未过期」才拒绝；其余一律放行
            Some(r) => r.locked_until.is_none_or(|u| u <= now),
            None => true,
        }
    }

    /// 记一次失败；返回本次失败后是否已进入锁定。
    pub fn record_failure(&self, key: &str) -> bool {
        let now = std::time::Instant::now();
        let locked = {
            let mut map = self.fails.lock().unwrap_or_else(|e| e.into_inner());
            let rec = map.entry(key.to_string()).or_insert(FailRecord {
                fails: 0,
                locked_until: None,
                last_fail: now,
            });
            // 锁定已过期的旧记录重新开始计
            if rec.locked_until.is_some_and(|u| u <= now) {
                rec.fails = 0;
                rec.locked_until = None;
            }
            rec.fails += 1;
            rec.last_fail = now;
            if rec.fails >= THROTTLE_MAX_FAILS {
                rec.locked_until =
                    Some(now + std::time::Duration::from_secs(THROTTLE_LOCK_MIN * 60));
                true
            } else {
                false
            }
        };
        self.prune();
        locked
    }

    /// 登录成功立即清零，避免把正常用户的手滑累积成锁定。
    pub fn record_success(&self, key: &str) {
        let mut map = self.fails.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(key);
    }

    /// 顺手清理过期记录，防止 map 无限增长（登录页是公网可达的）。
    /// 判据是「最后一次失败」是否已超过一个锁定时长——没锁定的零星失败
    /// 一个窗口后自然失去意义，锁定的记录则靠 `locked_until` 兜底再删。
    /// 注意 std Mutex 不可重入，调用前必须确保没有持有锁。
    fn prune(&self) {
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_secs(THROTTLE_LOCK_MIN * 60);
        let mut map = self.fails.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, r| {
            r.locked_until.is_some_and(|u| u > now) || now.duration_since(r.last_fail) < window
        });
    }
}

/// 认证中间件会把 [`AuthUser`] 写入请求扩展，Handler 直接以提取器参数使用。
impl<S> axum::extract::FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<AuthUser>().cloned().ok_or_else(|| {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                "unauthorized",
            )
                .into_response()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::from_env()
    }

    /// 会话长度现在以秒为单位（= 空闲窗口），不能再按小时算。
    #[test]
    fn token_ttl_matches_requested_seconds() {
        let c = cfg();
        let token = create_token(&c, 900, "admin", "admin").unwrap();
        let claims = verify_token(&c, &token).expect("刚签发的令牌应可验证");
        assert_eq!(claims.exp - claims.iat, 900);
        assert_eq!(claims.sub, "admin");
    }

    /// 空闲窗口一过，令牌必须真的失效（前端锁定之外的服务端兜底）。
    #[test]
    fn expired_token_is_rejected() {
        let c = cfg();
        // 留出 jsonwebtoken 默认的 60s 时钟偏移余量
        let token = create_token(&c, -120, "admin", "admin").unwrap();
        assert!(verify_token(&c, &token).is_none());
    }

    #[test]
    fn tampered_token_is_rejected() {
        let c = cfg();
        let token = create_token(&c, 900, "admin", "admin").unwrap();
        let broken = format!("{}x", &token[..token.len() - 1]);
        assert!(verify_token(&c, &broken).is_none());
    }

    /// 防爆破：阈值内放行、超限锁定、成功后清零、锁定期内即使口令正确也拒。
    #[test]
    fn login_throttle_locks_after_max_fails_then_recovers() {
        let th = LoginThrottle::default();
        let key = "10.0.0.9";
        for _ in 0..THROTTLE_MAX_FAILS - 1 {
            assert!(th.allow(key), "未达阈值应放行");
            assert!(!th.record_failure(key), "未达阈值不应锁定");
        }
        assert!(th.allow(key), "第 5 次失败前仍放行");
        assert!(th.record_failure(key), "第 5 次失败应触发锁定");
        assert!(!th.allow(key), "锁定期间必须拒绝");

        // 成功登录清零后恢复（模拟锁定过期：直接换一个新 key 太取巧，
        // 这里用 record_success 覆盖「锁定期间无法登录」的唯一出口——超时）
        let th2 = LoginThrottle::default();
        for _ in 0..THROTTLE_MAX_FAILS {
            th2.record_failure(key);
        }
        assert!(!th2.allow(key));
        th2.record_success(key);
        assert!(th2.allow(key), "成功后必须清零");
    }
}
