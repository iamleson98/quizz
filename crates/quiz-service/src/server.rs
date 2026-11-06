//! The two HTTP routers: the public one (the bank and the game socket) and
//! the internal one (probes, metrics, calls from the Go server).

use crate::auth::keys_match;
use crate::error::AppError;
use crate::repo::games;
use crate::rest::handlers;
use crate::state::AppState;
use crate::telemetry;
use crate::tx;
use crate::ws;
use axum::extract::{Path, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderName, HeaderValue, IF_NONE_MATCH,
};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Router};
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;
use std::time::Duration;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, Predicate, SizeAbove};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;

/// Largest request body: 100 questions of 6 options of 200 characters.
const BODY_LIMIT: usize = 512 * 1024;
/// Responses under this size are not worth compressing.
const COMPRESS_ABOVE: u64 = 256;
/// Browsers may cache a preflight answer this long.
const PREFLIGHT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Name of the header the Go server's internal calls carry the key in.
pub const INTERNAL_KEY_HEADER: &str = "x-internal-key";

/// `/v1/*` (the bank) and `/ws` (the game); nothing else.
pub fn public_router(state: AppState) -> Router {
    let origins: Vec<HeaderValue> = state
        .cfg
        .allowed_origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([AUTHORIZATION, CONTENT_TYPE, IF_NONE_MATCH])
        .expose_headers([ETAG])
        .max_age(PREFLIGHT_MAX_AGE);

    Router::new()
        .route(
            "/v1/quizzes",
            get(handlers::list_quizzes).post(handlers::create_quiz),
        )
        .route(
            "/v1/quizzes/{id}",
            get(handlers::get_quiz)
                .put(handlers::update_quiz)
                .delete(handlers::delete_quiz),
        )
        .route("/v1/quizzes/{id}/kept", put(handlers::keep_quiz))
        .route("/v1/quizzes/{id}/games", get(handlers::list_quiz_games))
        .route("/ws", get(ws::handler))
        .layer(RequestBodyLimitLayer::new(BODY_LIMIT))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        // Every response is `no-store` unless the handler said otherwise.
        .layer(SetResponseHeaderLayer::if_not_present(
            CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .layer(
            CompressionLayer::new()
                .compress_when(DefaultPredicate::new().and(SizeAbove::new(COMPRESS_ABOVE))),
        )
        .layer(cors)
        .layer(middleware::from_fn(telemetry::http_metrics_middleware))
        .with_state(state)
}

/// Probes, metrics and the calls only the Go server makes.
pub fn internal_router(state: AppState, metrics: PrometheusHandle) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
        .route("/internal/users/{id}/forget", post(forget))
        .layer(Extension(metrics))
        .with_state(state)
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "sqlite": state.sqlite_version,
        "tx_mode": state.cfg.tx_mode.as_str(),
        "uptime_sec": state.started_at.elapsed().as_secs(),
    }))
}

async fn readyz(State(state): State<AppState>) -> Response {
    match crate::db::ping(&state.pool).await {
        Ok(()) => Json(json!({"status": "ready"})).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable", "error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn metrics_handler(Extension(handle): Extension<PrometheusHandle>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        handle.render(),
    )
        .into_response()
}

/// Delete a person's quizzes and games, and blank their name on other
/// teachers' scoreboards. Called when an account is deleted.
async fn forget(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let given = headers
        .get(HeaderName::from_static(INTERNAL_KEY_HEADER))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !keys_match(&state.cfg.internal_key, given) {
        return Err(AppError::Unauthorized("bad internal key".into()));
    }
    if user_id.is_empty() || user_id.len() > 64 {
        return Err(AppError::BadRequest("not a user id".into()));
    }
    // Games running now first (a game the person hosts ends), then the rows.
    state.rooms.forget(&user_id).await;
    let forgotten = tx::write_tx(&state, move |conn| {
        let user_id = user_id.clone();
        Box::pin(async move { games::forget(conn, &user_id).await })
    })
    .await?;
    Ok(Json(json!({
        "quizzes": forgotten.quizzes,
        "players": forgotten.players,
    })))
}
