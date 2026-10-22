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
}
