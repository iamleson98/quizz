//! Extractors that turn every failure into the API's error body.

use crate::auth::{Ticket, ticket_from_headers};
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

/// The verified ticket of the caller.
pub struct Auth(pub Ticket);

impl FromRequestParts<AppState> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        ticket_from_headers(&state.ticket_key, &parts.headers)
            .map(Auth)
            .ok_or_else(|| AppError::Unauthorized("missing or invalid ticket".into()))
    }
}

/// A JSON body, with the API's errors.
pub struct ApiJson<T>(pub T);

impl<T: DeserializeOwned> FromRequest<AppState> for ApiJson<T> {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, AppError> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(ApiJson(v)),
            Err(JsonRejection::BytesRejection(e))
                if e.status() == StatusCode::PAYLOAD_TOO_LARGE =>
            {
                Err(AppError::TooLarge)
            }
            Err(e) => Err(AppError::BadRequest(e.body_text())),
        }
    }
}

/// Query parameters, with the API's errors.
pub struct ApiQuery<T>(pub T);

impl<T: DeserializeOwned> FromRequestParts<AppState> for ApiQuery<T> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|q| ApiQuery(q.0))
            .map_err(|e: QueryRejection| AppError::BadRequest(e.body_text()))
    }
}
