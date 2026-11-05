//! GraphQL schema, axum handlers and the HTTP router.

pub mod mutation;
pub mod query;
pub mod subscription;
pub mod types;

use crate::auth;
use crate::state::AppState;
use crate::telemetry;
use async_graphql::Schema;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware;
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;

pub type QuizSchema =
    Schema<query::QueryRoot, mutation::MutationRoot, subscription::SubscriptionRoot>;

pub fn build_schema(state: AppState) -> QuizSchema {
    Schema::build(
        query::QueryRoot,
        mutation::MutationRoot,
        subscription::SubscriptionRoot,
    )
    .data(state)
    // Per-operation tracing spans (picked up by the OTel layer).
    .extension(async_graphql::extensions::Tracing)
    .finish()
}

/// POST /graphql - the main entry point. Bearer tokens are resolved into an
/// [`crate::auth::AuthUser`] and attached to the request data.
async fn graphql_handler(
    State(state): State<AppState>,
    axum::Extension(schema): axum::Extension<QuizSchema>,
    headers: HeaderMap,
    req: async_graphql_axum::GraphQLRequest,
) -> async_graphql_axum::GraphQLResponse {
    let mut req = req.into_inner();
    if let Some(user) = auth::user_from_headers(&state.cfg, &headers) {
        req = req.data(user);
    }
    schema.execute(req).await.into()
}

/// GET /graphql/ws - subscriptions over `graphql-transport-ws`.
async fn graphql_ws_handler(
    axum::Extension(schema): axum::Extension<QuizSchema>,
    protocol: async_graphql_axum::GraphQLProtocol,
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    ws.protocols(async_graphql::http::ALL_WEBSOCKET_PROTOCOLS)
        .on_upgrade(move |stream| {
            async_graphql_axum::GraphQLWebSocket::new(stream, schema, protocol).serve()
        })
}

/// GET /playground - GraphQL Playground UI.
async fn playground_handler() -> Html<String> {
    Html(async_graphql::http::playground_source(
        async_graphql::http::GraphQLPlaygroundConfig::new("/graphql")
            .subscription_endpoint("/graphql/ws"),
    ))
}

async fn healthz_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "sqlite": state.sqlite_version,
        "tx_mode": state.cfg.tx_mode.as_str(),
        "uptime_sec": state.started_at.elapsed().as_secs(),
    }))
}

async fn readyz_handler(State(state): State<AppState>) -> impl IntoResponse {
    match crate::db::ping(&state.pool).await {
        Ok(()) => Json(json!({"status": "ready"})).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable", "error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn metrics_handler(axum::Extension(handle): axum::Extension<PrometheusHandle>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        handle.render(),
    )
        .into_response()
}

/// Build the complete HTTP router.
pub fn build_router(state: AppState, metrics: PrometheusHandle) -> Router {
    let schema = build_schema(state.clone());

    Router::new()
        .route("/graphql", post(graphql_handler))
        .route("/graphql/ws", get(graphql_ws_handler))
        .route("/playground", get(playground_handler))
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .route("/metrics", get(metrics_handler))
        .layer(axum::Extension(schema))
        .layer(axum::Extension(metrics))
        .layer(middleware::from_fn(telemetry::http_metrics_middleware))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(1 << 20))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(state)
}

/// Helper for tests: build a request to the GraphQL endpoint.
#[allow(dead_code)]
pub fn graphql_request(body: String) -> Request {
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    HttpRequest::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("build request")
}
