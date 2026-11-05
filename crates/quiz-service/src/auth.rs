//! Authentication: Argon2id password hashing and stateless JWT bearer
//! tokens resolved lazily per GraphQL request.

use crate::config::Config;
use crate::error::{AppError, AppResult};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::http::HeaderMap;
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

/// The authenticated principal attached to a GraphQL request.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub user_id: i64,
    pub username: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    username: String,
    iat: i64,
    exp: i64,
}

/// Hash a password with Argon2id (default params; auto random salt).
pub fn hash_password(password: &str) -> AppResult<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("password hashing failed: {e}")))
}

/// Verify a password against its Argon2 hash (constant time).
pub fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

/// Issue a signed JWT for the user.
pub fn issue_token(cfg: &Config, user_id: i64, username: &str) -> AppResult<String> {
    let now = jsonwebtoken::get_current_timestamp() as i64;
    let claims = Claims {
        sub: user_id.to_string(),
        username: username.to_string(),
        iat: now,
        exp: now + cfg.jwt_ttl_hours * 3600,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(cfg.jwt_secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("token signing failed: {e}")))
}

/// Resolve an `Authorization: Bearer <jwt>` header into an AuthUser.
pub fn user_from_headers(cfg: &Config, headers: &HeaderMap) -> Option<AuthUser> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(cfg.jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .ok()?;
    data.claims.sub.parse::<i64>().ok().map(|user_id| AuthUser {
        user_id,
        username: data.claims.username,
    })
}

/// Require an authenticated user from the GraphQL context.
pub fn require_user(ctx: &async_graphql::Context<'_>) -> AppResult<AuthUser> {
    ctx.data_opt::<AuthUser>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("missing or invalid bearer token".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg() -> Config {
        crate::config::Config {
            listen_addr: String::new(),
            database_path: Default::default(),
            jwt_secret: "unit-test-secret".into(),
            jwt_ttl_hours: 1,
            pool_max: 1,
            pool_min: 1,
            busy_timeout_ms: 100,
            tx_mode: crate::config::TxMode::Concurrent,
            tx_max_retries: 1,
            commit_mutex: true,
            vfs: "unix".into(),
            cache_size_kib: 1024,
            mmap_size: 0,
            otel_endpoint: None,
            log_format: "pretty".into(),
            seed_demo: false,
        }
    }

    #[test]
    fn token_roundtrip() {
        let cfg = test_cfg();
        let token = issue_token(&cfg, 42, "alice").unwrap();
        let user = user_from_token(&cfg, &token).unwrap();
        assert_eq!(user.user_id, 42);
        assert_eq!(user.username, "alice");
    }

    #[test]
    fn password_hash_and_verify() {
        let hash = hash_password("s3cret!").unwrap();
        assert!(verify_password("s3cret!", &hash));
        assert!(!verify_password("wrong", &hash));
    }

    fn user_from_token(cfg: &Config, token: &str) -> Option<AuthUser> {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        user_from_headers(cfg, &headers)
    }
}
