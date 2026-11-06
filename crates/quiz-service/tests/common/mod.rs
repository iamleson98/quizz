//! Integration test helpers: a fully wired service on a temp SQLite file.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use quiz_service::auth::{Role, Ticket};
use quiz_service::config::Config;
use quiz_service::state::AppState;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub const TICKET_KEY: &str = "integration-ticket-key-0123456789abcdef";
pub const INTERNAL_KEY: &str = "integration-internal-key-0123456789abc";
pub const SITE: &str = "https://site.example";

pub fn temp_path(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("quiz-it-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name)
}

/// The service's own configuration parsing, with the test's values.
pub fn config_with(extra: &[(&str, String)]) -> Config {
    let db = temp_path("quiz.db");
    let mut vars: Vec<(String, String)> = vec![
        ("QUIZ_DATABASE".into(), db.to_string_lossy().into()),
        ("QUIZ_TICKET_KEY".into(), TICKET_KEY.into()),
        ("QUIZ_INTERNAL_KEY".into(), INTERNAL_KEY.into()),
        ("QUIZ_ALLOWED_ORIGINS".into(), SITE.into()),
        ("QUIZ_SNAPSHOT_DIR".into(), String::new()),
        ("QUIZ_LMS_URL".into(), String::new()),
        // tests write a lot: the limiter has its own test
        ("QUIZ_BANK_WRITE_BURST".into(), "1000000".into()),
        ("QUIZ_BANK_WRITES_PER_MINUTE".into(), "1000000".into()),
    ];
    for (k, v) in extra {
        vars.retain(|(name, _)| name != k);
        vars.push((k.to_string(), v.clone()));
    }
    Config::from_lookup(|name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
        .expect("test config")
}

pub async fn setup() -> AppState {
    setup_with(&[]).await
}

pub async fn setup_with(extra: &[(&str, String)]) -> AppState {
    AppState::connect(config_with(extra))
        .await
        .expect("connect test db")
}

pub fn now_secs() -> i64 {
    jsonwebtoken::get_current_timestamp() as i64
}

/// A signed ticket for the test service.
pub fn ticket(state: &AppState, role: Role, sub: &str) -> String {
    ticket_for(state, role, sub, "s1")
}

pub fn ticket_for(state: &AppState, role: Role, sub: &str, sid: &str) -> String {
    let in_room = role != Role::Teacher;
    Ticket {
        sub: sub.into(),
        name: format!("Name {sub}"),
        role,
        sid: in_room.then(|| sid.to_string()),
        cid: in_room.then(|| "c1".to_string()),
        rc: in_room.then(|| state.now_ms() + 2 * 60 * 60 * 1000),
    }
    .sign(&state.cfg.ticket_key, now_secs(), now_secs() + 600)
    .expect("sign")
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
    pub raw: Vec<u8>,
}

/// One request through the router.
pub async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
    extra_headers: &[(&str, &str)],
) -> Reply {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    for (k, v) in extra_headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string())),
        None => b.body(Body::empty()),
    }
    .expect("request");
    let res = router.clone().oneshot(req).await.expect("oneshot");
    let (parts, body) = res.into_parts();
    let raw = body.collect().await.expect("body").to_bytes().to_vec();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: serde_json::from_slice(&raw).unwrap_or(Value::Null),
        raw,
    }
}

pub async fn get(router: &Router, uri: &str, token: &str) -> Reply {
    call(router, "GET", uri, Some(token), None, &[]).await
}

pub async fn post(router: &Router, uri: &str, token: &str, body: Value) -> Reply {
    call(router, "POST", uri, Some(token), Some(body), &[]).await
}

pub async fn put(router: &Router, uri: &str, token: &str, body: Value) -> Reply {
    call(router, "PUT", uri, Some(token), Some(body), &[]).await
}

pub async fn delete(router: &Router, uri: &str, token: &str) -> Reply {
    call(router, "DELETE", uri, Some(token), None, &[]).await
}

/// A valid three-question quiz body (single, multiple choice, true/false).
pub fn quiz_body(title: &str) -> Value {
    json!({
        "title": title,
        "description": "fractions",
        "questions": [
            { "kind": "single", "text": "1/2 + 1/2 = ?", "timeLimitSec": 20, "explanation": "one whole",
              "options": [
                { "text": "1", "isCorrect": true }, { "text": "2", "isCorrect": false },
                { "text": "0", "isCorrect": false }, { "text": "1/4", "isCorrect": false } ] },
            { "kind": "multi", "text": "Which are prime?",
              "options": [
                { "text": "2", "isCorrect": true }, { "text": "4", "isCorrect": false },
                { "text": "3", "isCorrect": true }, { "text": "9", "isCorrect": false } ] },
            { "kind": "boolean", "text": "0.5 = 1/2", "timeLimitSec": 10,
              "options": [ { "text": "True", "isCorrect": true }, { "text": "False", "isCorrect": false } ] }
        ]
    })
}
