use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::async_trait;
use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

// ---------------- 口令哈希（Argon2id） ----------------

pub fn hash_password(pw: &str) -> AppResult<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("hash: {e}")))
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(ph) => Argon2::default().verify_password(pw.as_bytes(), &ph).is_ok(),
        Err(_) => false,
    }
}

// ---------------- JWT ----------------

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: i64,
    pub username: String,
    pub exp: usize,
}

pub fn encode_token(cfg: &Config, user_id: i64, username: &str) -> AppResult<String> {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(12)).timestamp() as usize;
    let claims = Claims { sub: user_id, username: username.to_string(), exp };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(cfg.jwt_secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("jwt: {e}")))
}

pub fn decode_token(cfg: &Config, token: &str) -> AppResult<Claims> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(cfg.jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .map(|d| d.claims)
    .map_err(|_| AppError::Unauthorized("token 无效或已过期".into()))
}

// ---------------- 提取器 ----------------

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
}

/// 从 Authorization: Bearer <jwt> 解析当前用户；解析失败即 401
#[async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let raw = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| AppError::Unauthorized("缺少 Authorization 头".into()))?;
        let token = raw
            .strip_prefix("Bearer ")
            .ok_or_else(|| AppError::Unauthorized("Authorization 需为 Bearer <token>".into()))?;
        let claims = decode_token(&state.cfg, token)?;
        Ok(AuthUser { id: claims.sub, username: claims.username })
    }
}

/// WebSocket 无法带自定义头，改从 query 取 token
pub fn user_from_token(state: &AppState, token: &str) -> AppResult<AuthUser> {
    let claims = decode_token(&state.cfg, token)?;
    Ok(AuthUser { id: claims.sub, username: claims.username })
}