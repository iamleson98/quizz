//! `openapi.json` at the repository root is generated from the handlers and
//! types; this test fails when the committed file is out of date.
//! Regenerate with `UPDATE_OPENAPI=1 cargo test -p quiz-service --test openapi`.

use quiz_service::rest::docs::openapi_json;
use serde_json::Value;

fn committed_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../openapi.json")
}

#[test]
fn the_committed_openapi_document_is_current() {
    let generated = openapi_json();
    let path = committed_path();
    if std::env::var("UPDATE_OPENAPI").is_ok_and(|v| v == "1") {
        std::fs::write(&path, &generated).expect("write openapi.json");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "openapi.json is out of date: run `UPDATE_OPENAPI=1 cargo test -p quiz-service --test openapi`"
    );
}

#[test]
fn every_operation_has_an_id_a_tag_security_and_error_responses() {
    let doc: Value = serde_json::from_str(&openapi_json()).unwrap();
    let mut ids = Vec::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            let id = op["operationId"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} {path} has no operationId"));
            ids.push(id.to_string());
            assert!(!op["tags"].as_array().unwrap().is_empty(), "{id} is tagged");
            assert_eq!(
                op["security"][0]["bearer"],
                serde_json::json!([]),
                "{id} needs the bearer ticket"
            );
            let codes: Vec<&String> = op["responses"].as_object().unwrap().keys().collect();
            assert!(codes.iter().any(|c| *c == "401"), "{id} describes 401");
            assert!(codes.iter().any(|c| *c == "403"), "{id} describes 403");
        }
    }
    ids.sort();
    assert_eq!(
        ids,
        [
            "createQuiz",
            "deleteQuiz",
            "getQuiz",
            "keepQuiz",
            "listQuizGames",
            "listQuizzes",
            "updateQuiz"
        ]
    );
    assert_eq!(
        doc["components"]["securitySchemes"]["bearer"]["scheme"],
        "bearer"
    );
}
