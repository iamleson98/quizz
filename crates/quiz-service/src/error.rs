//! Unified application error type, convertible into GraphQL errors with
//! `code` extensions and into busy-detection for the transaction engine.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden")]
    Forbidden,

    #[error("{0} not found")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("transaction retries exhausted: {0}")]
    TxExhausted(String),

    #[error("internal error: {0}")]
    Internal(String),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    /// Stable machine-readable error code (GraphQL `extensions.code`).
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Unauthorized(_) => "UNAUTHORIZED",
            AppError::Forbidden => "FORBIDDEN",
            AppError::NotFound(_) => "NOT_FOUND",
            AppError::BadRequest(_) => "BAD_REQUEST",
            AppError::Conflict(_) => "CONFLICT",
            AppError::Database(_) => "DATABASE_ERROR",
            AppError::Migrate(_) => "MIGRATION_ERROR",
            AppError::TxExhausted(_) => "TX_EXHAUSTED",
            AppError::Internal(_) => "INTERNAL_ERROR",
        }
    }
}

/// GraphQL-facing error wrapper.
///
/// async-graphql blanket-implements `From<T: Display> for Error`, so we
/// cannot implement `From<AppError> for Error` directly without losing the
/// `code` extension. This wrapper carries the enriched error through
/// resolver signatures (`Result<T, GqlError>`).
pub struct GqlError(async_graphql::Error);

impl From<AppError> for GqlError {
    fn from(err: AppError) -> Self {
        use async_graphql::ErrorExtensions;
        let code = err.code();
        let message = err.to_string();
        GqlError(async_graphql::Error::new(message).extend_with(|_, e| e.set("code", code)))
    }
}

impl From<async_graphql::Error> for GqlError {
    fn from(err: async_graphql::Error) -> Self {
        GqlError(err)
    }
}

impl From<GqlError> for async_graphql::Error {
    fn from(err: GqlError) -> Self {
        err.0
    }
}

/// True when the sqlx/SQLite error is a lock contention signal
/// (SQLITE_BUSY, SQLITE_BUSY_SNAPSHOT, SQLITE_LOCKED, ...).
pub fn is_busy_error(err: &sqlx::Error) -> bool {
    use sqlx::Error as E;
    match err {
        E::Database(db) => {
            // SQLite primary result codes: SQLITE_BUSY = 5, SQLITE_LOCKED = 6.
            // Extended codes (517 = SQLITE_BUSY_SNAPSHOT, 261 = BUSY_RECOVERY,
            // 262 = LOCKED_SHAREDCACHE) share the low byte with the primary.
            let primary = db
                .code()
                .and_then(|c| c.parse::<i64>().ok())
                .map(|c| c & 0xff);
            if matches!(primary, Some(5) | Some(6)) {
                return true;
            }
            let msg = db.message().to_ascii_lowercase();
            msg.contains("database is locked")
                || msg.contains("database table is locked")
                || msg.contains("busy")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphql_conversion_sets_code() {
        let err: async_graphql::Error = GqlError::from(AppError::NotFound("quiz".into())).into();
        let code = err.extensions.expect("extensions").get("code").cloned();
        assert_eq!(code, Some(async_graphql::Value::from("NOT_FOUND")));
    }
}
