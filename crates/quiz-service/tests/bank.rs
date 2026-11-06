//! The bank API through the public router: tickets, ownership, validation,
//! ETags, CORS, compression, and what the public listener does not serve.

mod common;

use axum::http::StatusCode;
use common::*;
use quiz_service::auth::Role;
use quiz_service::server::public_router;
use serde_json::{Value, json};

async fn created(router: &axum::Router, token: &str, title: &str) -> Value {
    let r = post(router, "/v1/quizzes", token, quiz_body(title)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body
}

#[tokio::test]
async fn create_read_list_roundtrip_with_keys_for_the_owner() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");

    let quiz = created(&router, &t, "Fractions").await;
    assert_eq!(quiz["title"], "Fractions");
    assert_eq!(
        quiz["kept"], true,
        "a quiz made in the bank is kept by default"
    );
    assert_eq!(quiz["questionCount"], 3);
    assert!(
        quiz["id"].as_str().unwrap().parse::<i64>().is_ok(),
        "ids are strings"
    );
    let q0 = &quiz["questions"][0];
    assert_eq!(q0["timeLimitSec"], 20);
    assert_eq!(q0["explanation"], "one whole");
    assert_eq!(
        q0["options"][0]["isCorrect"], true,
        "the owner reads the keys"
    );
    assert_eq!(
        quiz["questions"][1]["timeLimitSec"], 20,
        "the default limit is 20 s"
    );

    let id = quiz["id"].as_str().unwrap();
    let got = get(&router, &format!("/v1/quizzes/{id}"), &t).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.body, quiz);

    let list = get(&router, "/v1/quizzes", &t).await;
    assert_eq!(list.body["total"], 1);
    assert_eq!(list.body["items"][0]["questionCount"], 3);
    assert!(list.body["items"][0].get("questions").is_none());
}

#[tokio::test]
async fn lists_are_paged_newest_first_and_limits_are_clamped() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    for i in 0..3 {
        created(&router, &t, &format!("Quiz {i}")).await;
        state.clock.advance_ms(10);
    }
    let page = get(&router, "/v1/quizzes?limit=2&offset=0", &t).await;
    assert_eq!(page.body["total"], 3);
    assert_eq!(page.body["items"].as_array().unwrap().len(), 2);
    assert_eq!(page.body["items"][0]["title"], "Quiz 2");
    let rest = get(&router, "/v1/quizzes?limit=2&offset=2", &t).await;
    assert_eq!(rest.body["items"][0]["title"], "Quiz 0");
    let big = get(&router, "/v1/quizzes?limit=100000", &t).await;
    assert_eq!(big.status, StatusCode::OK, "a huge limit is clamped to 100");
    let bad = get(&router, "/v1/quizzes?limit=abc", &t).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.body["code"], "BAD_REQUEST");
}

#[tokio::test]
async fn tickets_roles_and_ownership() {
    let state = setup().await;
    let router = public_router(state.clone());
    let teacher = ticket(&state, Role::Teacher, "teacher1");
    let host = ticket(&state, Role::Host, "teacher1");
    let player = ticket(&state, Role::Player, "student1");
    let other = ticket(&state, Role::Teacher, "teacher2");
    let id = created(&router, &teacher, "Mine").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/quizzes/{id}");

    let none = call(&router, "GET", "/v1/quizzes", None, None, &[]).await;
    assert_eq!(
        (none.status, none.body["code"].clone()),
        (StatusCode::UNAUTHORIZED, json!("UNAUTHORIZED"))
    );
    let junk = get(&router, "/v1/quizzes", "not-a-ticket").await;
    assert_eq!(junk.status, StatusCode::UNAUTHORIZED);

    let denied = get(&router, "/v1/quizzes", &player).await;
    assert_eq!(
        (denied.status, denied.body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("FORBIDDEN"))
    );
    assert_eq!(
        post(&router, "/v1/quizzes", &player, quiz_body("x"))
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // a host reaches the same bank as the teacher
    assert_eq!(get(&router, &uri, &host).await.status, StatusCode::OK);

    // another owner's quiz does not exist for you
    for r in [
        get(&router, &uri, &other).await,
        put(&router, &uri, &other, quiz_body("x")).await,
        put(
            &router,
            &format!("{uri}/kept"),
            &other,
            json!({"kept": false}),
        )
        .await,
        delete(&router, &uri, &other).await,
        get(&router, &format!("{uri}/games"), &other).await,
    ] {
        assert_eq!(
            (r.status, r.body["code"].clone()),
            (StatusCode::NOT_FOUND, json!("NOT_FOUND"))
        );
    }
    assert_eq!(get(&router, "/v1/quizzes", &other).await.body["total"], 0);
    assert_eq!(
        get(&router, "/v1/quizzes/not-a-number", &teacher)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn validation_reports_every_problem_on_its_field() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let mut body = quiz_body("");
    body["questions"][1]["options"][1]["text"] = json!("");
    body["questions"][2]["options"] = json!([
        { "text": "a", "isCorrect": true }, { "text": "b", "isCorrect": false }, { "text": "c", "isCorrect": false }
    ]);
    body["questions"][0]["timeLimitSec"] = json!(4);
    let r = post(&router, "/v1/quizzes", &t, body).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["code"], "INVALID");
    let paths: Vec<&str> = r.body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    for expected in [
        "title",
        "questions[0].timeLimitSec",
        "questions[1].options[1].text",
        "questions[2].options",
    ] {
        assert!(paths.contains(&expected), "{expected} in {paths:?}");
    }
    assert!(r.body["fields"][0]["message"].as_str().unwrap().len() > 3);
}

#[tokio::test]
async fn every_limit_of_the_design_is_enforced() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let q = |opts: usize, text: &str| {
        let options: Vec<Value> = (0..opts)
            .map(|i| json!({ "text": format!("o{i}"), "isCorrect": i == 0 }))
            .collect();
        json!({ "kind": "single", "text": text, "options": options })
    };
    let body = |questions: Vec<Value>| json!({ "title": "t", "questions": questions });

    let too_many: Vec<Value> = (0..101).map(|_| q(2, "q")).collect();
    for (name, b) in [
        ("101 questions", body(too_many)),
        ("no questions", body(vec![])),
        ("501-character question", body(vec![q(2, &"x".repeat(501))])),
        ("one option", body(vec![q(1, "q")])),
        ("seven options", body(vec![q(7, "q")])),
        (
            "201-character option",
            body(vec![json!({"kind":"single","text":"q","options":[
            {"text":"x".repeat(201),"isCorrect":true},{"text":"b","isCorrect":false}]})]),
        ),
    ] {
        assert_eq!(
            post(&router, "/v1/quizzes", &t, b).await.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{name}"
        );
    }
    for ok in [
        body((0..100).map(|_| q(2, "q")).collect()),
        body(vec![q(6, &"x".repeat(500))]),
    ] {
        assert_eq!(
            post(&router, "/v1/quizzes", &t, ok).await.status,
            StatusCode::CREATED
        );
    }
    let mut long = body(vec![q(2, "q")]);
    long["questions"][0]["timeLimitSec"] = json!(121);
    assert_eq!(
        post(&router, "/v1/quizzes", &t, long).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn at_most_200_quizzes_per_teacher() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let small = json!({ "title": "t", "questions": [ { "kind": "boolean", "text": "q",
        "options": [ { "text": "a", "isCorrect": true }, { "text": "b", "isCorrect": false } ] } ] });
    for _ in 0..200 {
        assert_eq!(
            post(&router, "/v1/quizzes", &t, small.clone()).await.status,
            StatusCode::CREATED
        );
    }
    let r = post(&router, "/v1/quizzes", &t, small.clone()).await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (StatusCode::CONFLICT, json!("CONFLICT"))
    );
    let other = ticket(&state, Role::Teacher, "teacher2");
    assert_eq!(
        post(&router, "/v1/quizzes", &other, small).await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn malformed_and_oversized_bodies() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let bad = call(
        &router,
        "POST",
        "/v1/quizzes",
        Some(&t),
        None,
        &[("content-type", "application/json")],
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let missing = post(&router, "/v1/quizzes", &t, json!({ "title": "x" })).await;
    assert_eq!(
        (missing.status, missing.body["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("BAD_REQUEST"))
    );
    let big = post(
        &router,
        "/v1/quizzes",
        &t,
        json!({ "title": "x".repeat(600_000), "questions": [] }),
    )
    .await;
    assert_eq!(
        (big.status, big.body["code"].clone()),
        (StatusCode::PAYLOAD_TOO_LARGE, json!("BAD_REQUEST"))
    );
}

#[tokio::test]
async fn saving_a_quiz_keeps_the_identity_of_what_stays() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let quiz = created(&router, &t, "v1").await;
    let id = quiz["id"].as_str().unwrap();
    let mut edit = quiz.clone();
    edit["title"] = json!("v2");
    edit["questions"].as_array_mut().unwrap().remove(2); // drop the true/false
    edit["questions"][0]["options"][1]["text"] = json!("two");
    edit["questions"][0]["options"]
        .as_array_mut()
        .unwrap()
        .pop();
    edit["questions"].as_array_mut().unwrap().push(json!({
        "kind": "boolean", "text": "new", "options": [ { "text": "T", "isCorrect": true }, { "text": "F", "isCorrect": false } ]
    }));
    let saved = put(&router, &format!("/v1/quizzes/{id}"), &t, edit.clone()).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["title"], "v2");
    assert_eq!(
        saved.body["questions"][0]["id"], quiz["questions"][0]["id"],
        "same question"
    );
    assert_eq!(
        saved.body["questions"][0]["options"][0]["id"],
        quiz["questions"][0]["options"][0]["id"]
    );
    assert_eq!(saved.body["questions"][0]["options"][1]["text"], "two");
    assert_eq!(
        saved.body["questions"][0]["options"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(saved.body["questions"][1]["id"], quiz["questions"][1]["id"]);
    assert_ne!(
        saved.body["questions"][2]["id"], quiz["questions"][2]["id"],
        "a new question"
    );
    assert_eq!(saved.body["questionCount"], 3);

    let mut foreign = saved.body.clone();
    foreign["questions"][0]["id"] = json!("12345");
    let r = put(&router, &format!("/v1/quizzes/{id}"), &t, foreign).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["fields"][0]["path"], "questions[0].id");
}

#[tokio::test]
async fn etags_make_reopening_a_304() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let quiz = created(&router, &t, "Q").await;
    let uri = format!("/v1/quizzes/{}", quiz["id"].as_str().unwrap());

    let first = get(&router, &uri, &t).await;
    let etag = first.headers["etag"].to_str().unwrap().to_string();
    assert_eq!(first.headers["cache-control"], "private, no-cache");
    let again = call(
        &router,
        "GET",
        &uri,
        Some(&t),
        None,
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.raw.is_empty());
    assert_eq!(again.headers["etag"].to_str().unwrap(), etag);

    // an edit changes it
    state.clock.advance_ms(5);
    put(&router, &uri, &t, quiz_body("changed")).await;
    let after = call(
        &router,
        "GET",
        &uri,
        Some(&t),
        None,
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(after.status, StatusCode::OK);
    assert_ne!(after.headers["etag"].to_str().unwrap(), etag);

    // everything else is no-store
    assert_eq!(
        get(&router, "/v1/quizzes", &t).await.headers["cache-control"],
        "no-store"
    );
    assert_eq!(
        post(&router, "/v1/quizzes", &t, quiz_body("n"))
            .await
            .headers["cache-control"],
        "no-store"
    );
    assert_eq!(
        get(&router, "/v1/quizzes", "bad").await.headers["cache-control"],
        "no-store"
    );
}

#[tokio::test]
async fn delete_is_immediate_and_takes_the_games_with_it() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let id = created(&router, &t, "Q").await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/quizzes/{id}");
    assert_eq!(
        delete(&router, &uri, &t).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(get(&router, &uri, &t).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        delete(&router, &uri, &t).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn cors_answers_the_sites_origins_only_and_caches_preflights_for_a_day() {
    let state = setup().await;
    let router = public_router(state);
    async fn pre(router: &axum::Router, origin: &str) -> Reply {
        call(
            router,
            "OPTIONS",
            "/v1/quizzes",
            None,
            None,
            &[
                ("origin", origin),
                ("access-control-request-method", "POST"),
                (
                    "access-control-request-headers",
                    "authorization,content-type",
                ),
            ],
        )
        .await
    }
    let ok = pre(&router, SITE).await;
    assert_eq!(ok.headers["access-control-allow-origin"], SITE);
    assert_eq!(ok.headers["access-control-max-age"], "86400");
    let allowed = ok.headers["access-control-allow-headers"]
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    assert!(allowed.contains("authorization") && allowed.contains("content-type"));
    assert!(
        pre(&router, "https://evil.example")
            .await
            .headers
            .get("access-control-allow-origin")
            .is_none()
    );
    let simple = call(
        &router,
        "GET",
        "/v1/quizzes",
        None,
        None,
        &[("origin", SITE)],
    )
    .await;
    assert_eq!(
        simple.headers["access-control-allow-origin"], SITE,
        "errors carry CORS too"
    );
    assert_eq!(simple.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn responses_are_compressed_above_256_bytes_only() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    created(&router, &t, "Compressible").await;
    let id = get(&router, "/v1/quizzes", &t).await.body["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/quizzes/{id}");
    for (accept, encoding) in [("gzip", "gzip"), ("br", "br"), ("zstd", "zstd")] {
        let r = call(
            &router,
            "GET",
            &uri,
            Some(&t),
            None,
            &[("accept-encoding", accept)],
        )
        .await;
        assert_eq!(r.headers["content-encoding"], encoding, "{accept}");
    }
    let plain = call(&router, "GET", &uri, Some(&t), None, &[]).await;
    assert!(plain.headers.get("content-encoding").is_none());
    let small = call(
        &router,
        "GET",
        "/v1/quizzes/1",
        Some(&t),
        None,
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert!(
        small.raw.len() < 256 && small.headers.get("content-encoding").is_none(),
        "tiny bodies stay plain"
    );
}

#[tokio::test]
async fn the_public_listener_serves_only_the_bank_and_the_socket() {
    let state = setup().await;
    let router = public_router(state);
    for path in [
        "/graphql",
        "/playground",
        "/metrics",
        "/healthz",
        "/readyz",
        "/internal/users/x/forget",
    ] {
        let r = call(&router, "GET", path, None, None, &[]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path}");
    }
    let post_forget = call(&router, "POST", "/internal/users/x/forget", None, None, &[]).await;
    assert_eq!(post_forget.status, StatusCode::NOT_FOUND);
}
