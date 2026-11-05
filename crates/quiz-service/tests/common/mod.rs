//! Integration test helpers: build a fully-wired app on a temp SQLite file.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use quiz_service::config::{Config, TxMode};
use quiz_service::state::AppState;
use quiz_service::telemetry;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn test_config() -> Config {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path: PathBuf =
        std::env::temp_dir().join(format!("quiz-it-{}-{}.db", std::process::id(), n));
    // clean leftovers from previous runs
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));

    Config {
        listen_addr: "127.0.0.1:0".into(),
        database_path: path,
        jwt_secret: "integration-test-secret".into(),
        jwt_ttl_hours: 1,
        pool_max: 8,
        pool_min: 2,
        busy_timeout_ms: 5000,
        tx_mode: TxMode::Concurrent,
        tx_max_retries: 64,
        commit_mutex: true,
        vfs: "unix".into(),
        cache_size_kib: 8192,
        mmap_size: 0,
        otel_endpoint: None,
        log_format: "pretty".into(),
        seed_demo: false,
    }
}

/// Build the app router backed by a fresh database.
pub async fn setup() -> (Router, AppState) {
    let cfg = test_config();
    let state = AppState::connect(cfg).await.expect("connect test db");
    let metrics = telemetry::init_metrics();
    let router = quiz_service::graphql::build_router(state.clone(), metrics);
    (router, state)
}

/// POST a GraphQL operation, optionally authenticated.
pub async fn gql(router: &Router, token: Option<&str>, query: &str, variables: Value) -> Value {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("content-type", "application/json");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let req = builder
        .body(Body::from(
            json!({ "query": query, "variables": variables }).to_string(),
        ))
        .expect("request");
    let res = router.clone().oneshot(req).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK, "graphql transport failure");
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    serde_json::from_slice(&bytes).expect("json response")
}

/// GET an arbitrary endpoint, returns (status, body string).
pub async fn get(router: &Router, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let res = router.clone().oneshot(req).await.expect("oneshot");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Register a user, returning (token, user_id).
pub async fn register(router: &Router, username: &str) -> (String, String) {
    let res = gql(
        router,
        None,
        r#"mutation($u: String!, $p: String!) { register(username: $u, password: $p) { token user { id username } } }"#,
        json!({ "u": username, "p": "password123" }),
    )
    .await;
    assert!(res["errors"].is_null(), "register failed: {res}");
    (
        res["data"]["register"]["token"]
            .as_str()
            .unwrap()
            .to_string(),
        res["data"]["register"]["user"]["id"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

/// Extract the first error code, if any.
pub fn error_code(res: &Value) -> Option<String> {
    res["errors"][0]["extensions"]["code"]
        .as_str()
        .map(String::from)
}

/// Extract a nested value; numeric segments index into arrays.
pub fn data(res: &Value, path: &str) -> Value {
    let mut cur = &res["data"];
    for seg in path.split('.') {
        if let Ok(idx) = seg.parse::<usize>() {
            cur = cur.get(idx).unwrap_or(&Value::Null);
        } else {
            cur = &cur[seg];
        }
    }
    cur.clone()
}
