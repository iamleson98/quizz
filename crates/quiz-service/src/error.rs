//! Unified application error, rendered as the REST error body
//! `{code, message, fields?}` with the matching HTTP status, and
//! busy-detection for the transaction engine.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;
use utoipa::ToSchema;

/// One validation problem, on the field it belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct FieldError {
    /// Where, like `questions[2].options[1].text`.
    pub path: String,
    pub message: String,
}

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

    #[error("request body too large")]
    TooLarge,

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("the input is not valid")]
    Invalid(Vec<FieldError>),

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
    /// Stable machine-readable error code (the `code` of the error body).
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Unauthorized(_) => "UNAUTHORIZED",
            AppError::Forbidden => "FORBIDDEN",
            AppError::NotFound(_) => "NOT_FOUND",
            AppError::BadRequest(_) | AppError::TooLarge => "BAD_REQUEST",
            AppError::Conflict(_) => "CONFLICT",
            AppError::Invalid(_) => "INVALID",
            AppError::Database(_)
            | AppError::Migrate(_)
            | AppError::TxExhausted(_)
            | AppError::Internal(_) => "INTERNAL",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::TxExhausted(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Database(_) | AppError::Migrate(_) | AppError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// The numeric code of the game socket's `error` frame.
    pub fn frame_code(&self) -> u8 {
        match self {
            AppError::Unauthorized(_) => 1,
            AppError::Forbidden => 2,
            AppError::NotFound(_) => 3,
            AppError::BadRequest(_) | AppError::TooLarge | AppError::Invalid(_) => 4,
            AppError::Conflict(_) => 5,
            _ => 9,
        }
    }
}

/// The body of every error response.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    /// One of UNAUTHORIZED, FORBIDDEN, NOT_FOUND, BAD_REQUEST, CONFLICT,
    /// INVALID, INTERNAL.
    pub code: String,
    pub message: String,
    /// Present on INVALID: what is wrong and where.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<FieldError>>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        // Internals stay in the log: the caller only learns that it failed.
        let message = match &self {
            AppError::Database(_) | AppError::Migrate(_) | AppError::Internal(_) => {
                "internal error".to_string()
            }
            AppError::TxExhausted(_) => "the service is busy, try again".to_string(),
            other => other.to_string(),
        };
        let body = ErrorBody {
            code: self.code().into(),
            message,
            fields: match self {
                AppError::Invalid(f) => Some(f),
                _ => None,
            },
        };
        (status, Json(body)).into_response()
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
    fn statuses_and_codes_line_up() {
        let cases: Vec<(AppError, StatusCode, &str)> = vec![
            (
                AppError::Unauthorized("x".into()),
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
            ),
            (AppError::Forbidden, StatusCode::FORBIDDEN, "FORBIDDEN"),
            (
                AppError::NotFound("quiz".into()),
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
            ),
            (
                AppError::BadRequest("x".into()),
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
            ),
            (
                AppError::TooLarge,
                StatusCode::PAYLOAD_TOO_LARGE,
                "BAD_REQUEST",
            ),
            (
                AppError::Conflict("x".into()),
                StatusCode::CONFLICT,
                "CONFLICT",
            ),
            (
                AppError::Invalid(vec![]),
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID",
            ),
        ];
        for (err, status, code) in cases {
            assert_eq!(err.status(), status);
            assert_eq!(err.code(), code);
        }
    }
}
