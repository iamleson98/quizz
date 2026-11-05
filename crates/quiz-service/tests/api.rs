//! End-to-end integration tests over the real axum router + custom SQLite
//! (begin-concurrent) on temporary database files.

mod common;

use common::{data, error_code, get, gql, register, setup};
use serde_json::{Value, json};

/// Author creates a mixed quiz; a player plays it; scores and leaderboard
/// are verified end to end.
#[tokio::test]
async fn full_play_flow() {
    let (router, _state) = setup().await;
    let (author_tok, _author_id) = register(&router, "author").await;
    let (player_tok, _player_id) = register(&router, "player").await;

    // --- create quiz -----------------------------------------------------
    let res = gql(
        &router,
        Some(&author_tok),
        r#"mutation { createQuiz(input: { title: "Rust Basics", description: "d", isPublic: true }) { id title questionCount } }"#,
        json!({}),
    )
    .await;
    assert!(res["errors"].is_null(), "{res}");
    let quiz_id = data(&res, "createQuiz.id").as_str().unwrap().to_string();
    assert_eq!(data(&res, "createQuiz.questionCount"), json!(0));

    // --- add questions ----------------------------------------------------
    // single (2 pts, correct = "Mars"), multi (2 pts, correct = {2,13}),
    // boolean (1 pt, correct = "True")
    let res = gql(
        &router,
        Some(&author_tok),
        r#"mutation($quiz: ID!) {
            addQuestions(quizId: $quiz, input: [
                { kind: SINGLE, text: "Red planet?", points: 2,
                  options: [ {text: "Venus", isCorrect: false}, {text: "Mars", isCorrect: true} ] },
                { kind: MULTI, text: "Primes?", points: 2,
                  options: [ {text: "2", isCorrect: true}, {text: "9", isCorrect: false},
                             {text: "13", isCorrect: true}, {text: "15", isCorrect: false} ] },
                { kind: BOOLEAN, text: "Pacific is largest?", points: 1,
                  options: [ {text: "True", isCorrect: true}, {text: "False", isCorrect: false} ] },
            ]) { id position kind points }
        }"#,
        json!({ "quiz": quiz_id }),
    )
    .await;
    assert!(res["errors"].is_null(), "addQuestions failed: {res}");
    let questions = data(&res, "addQuestions").as_array().unwrap().clone();
    assert_eq!(questions.len(), 3);
    let q1 = questions[0]["id"].as_str().unwrap().to_string();
    let q2 = questions[1]["id"].as_str().unwrap().to_string();
    let q3 = questions[2]["id"].as_str().unwrap().to_string();

    // --- player fetches quiz: answers hidden, options shuffled-safe -------
    let res = gql(
        &router,
        Some(&player_tok),
        r#"query($id: ID!) { quiz(id: $id) { quiz { title questionCount } questions { id options { id isCorrect } } } }"#,
        json!({ "id": quiz_id }),
    )
    .await;
    assert!(res["errors"].is_null(), "{res}");
    assert_eq!(data(&res, "quiz.quiz.questionCount"), json!(3));
    let hidden = data(&res, "quiz.questions").as_array().unwrap().clone();
    for q in &hidden {
        for o in q["options"].as_array().unwrap() {
            assert!(
                o["isCorrect"].is_null(),
                "correctness must be hidden from players"
            );
        }
    }

    // resolve option ids per question
    let opts = |qi: usize| -> Vec<String> {
        hidden[qi]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["id"].as_str().unwrap().to_string())
            .collect()
    };
    let o1 = opts(0); // [Venus, Mars] (insertion order = position order)
    let o2 = opts(1); // [2, 9, 13, 15]
    let o3 = opts(2); // [True, False]

    // --- creator sees correctness ----------------------------------------
    let res = gql(
        &router,
        Some(&author_tok),
        r#"query($id: ID!) { quiz(id: $id) { questions { options { isCorrect } } } }"#,
        json!({ "id": quiz_id }),
    )
    .await;
    assert_eq!(
        data(&res, "quiz.questions.0.options.1.isCorrect"),
        json!(true),
        "creator must see correctness"
    );

    // --- player plays ------------------------------------------------------
    let res = gql(
        &router,
        Some(&player_tok),
        r#"mutation($quiz: ID!) { startAttempt(quizId: $quiz) { id status } }"#,
        json!({ "quiz": quiz_id }),
    )
    .await;
    assert!(res["errors"].is_null(), "{res}");
    let attempt_id = data(&res, "startAttempt.id").as_str().unwrap().to_string();
    assert_eq!(data(&res, "startAttempt.status"), json!("in_progress"));

    // answers: single correct (Mars = o1[1]); multi partial (only "13" of {2,13});
    // boolean wrong (False).
    let res = gql(
        &router,
        Some(&player_tok),
        r#"mutation($a: ID!, $ans: [AnswerInput!]!) {
            submitAttempt(attemptId: $a, answers: $ans) {
                score maxScore correctCount totalCount
                answers { correct awarded possible }
            }
        }"#,
        json!({ "a": attempt_id, "ans": [
            { "questionId": q1, "selectedOptionIds": [o1[1].clone()] },
            { "questionId": q2, "selectedOptionIds": [o2[2].clone()] },
            { "questionId": q3, "selectedOptionIds": [o3[1].clone()] },
        ]}),
    )
    .await;
    assert!(res["errors"].is_null(), "submit failed: {res}");
    // single: 2; multi: 2 * (1-0)/2 = 1; boolean: 0 => 3 of 5.
    // correctCount counts fully-correct answers: only the single one.
    assert_eq!(data(&res, "submitAttempt.score"), json!(3.0));
    assert_eq!(data(&res, "submitAttempt.maxScore"), json!(5.0));
    assert_eq!(data(&res, "submitAttempt.correctCount"), json!(1));
    assert_eq!(data(&res, "submitAttempt.totalCount"), json!(3));

    // double submit rejected
    let res = gql(
        &router,
        Some(&player_tok),
        r#"mutation($a: ID!, $ans: [AnswerInput!]!) { submitAttempt(attemptId: $a, answers: $ans) { score } }"#,
        json!({ "a": attempt_id, "ans": [] }),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("CONFLICT"));

    // --- leaderboard --------------------------------------------------------
    let res = gql(
        &router,
        None,
        r#"query($q: ID!) { leaderboard(quizId: $q, first: 10) { rank user { username } score } }"#,
        json!({ "q": quiz_id }),
    )
    .await;
    assert!(res["errors"].is_null(), "{res}");
    let lb = data(&res, "leaderboard").as_array().unwrap().clone();
    assert_eq!(lb.len(), 1);
    assert_eq!(lb[0]["rank"], json!(1));
    assert_eq!(lb[0]["user"]["username"], json!("player"));
    assert_eq!(lb[0]["score"], json!(3.0));

    // --- myAttempts + attempt detail ---------------------------------------
    let res = gql(
        &router,
        Some(&player_tok),
        r#"query($q: ID) { myAttempts(quizId: $q) { id quizId status score maxScore } }"#,
        json!({ "q": quiz_id }),
    )
    .await;
    assert_eq!(data(&res, "myAttempts.0.status"), json!("graded"));
    assert_eq!(data(&res, "myAttempts.0.score"), json!(3.0));

    let res = gql(
        &router,
        Some(&player_tok),
        r#"query($a: ID!) { attempt(id: $a) { attempt { score } answers { questionId correct awarded } } }"#,
        json!({ "a": attempt_id }),
    )
    .await;
    assert!(res["errors"].is_null(), "{res}");
    assert_eq!(
        data(&res, "attempt.answers.1.awarded"),
        json!(1.0),
        "partial credit"
    );
}

#[tokio::test]
async fn auth_and_ownership() {
    let (router, _s) = setup().await;
    let (a_tok, _a) = register(&router, "owner1").await;
    let (b_tok, _b) = register(&router, "intruder1").await;

    // me without token
    let res = gql(&router, None, r#"{ me { username } }"#, json!({})).await;
    assert_eq!(error_code(&res).as_deref(), Some("UNAUTHORIZED"));

    // me with garbage token
    let res = gql(
        &router,
        Some("garbage.token.here"),
        r#"{ me { username } }"#,
        json!({}),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("UNAUTHORIZED"));

    // me with valid token
    let res = gql(&router, Some(&a_tok), r#"{ me { username } }"#, json!({})).await;
    assert_eq!(data(&res, "me.username"), json!("owner1"));

    // duplicate username
    let res = gql(
        &router,
        None,
        r#"mutation { register(username: "OWNER1", password: "password123") { token } }"#,
        json!({}),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("CONFLICT"));

    // private quiz invisible to others
    let res = gql(
        &router,
        Some(&a_tok),
        r#"mutation { createQuiz(input: { title: "Secret", isPublic: false }) { id } }"#,
        json!({}),
    )
    .await;
    let quiz_id = data(&res, "createQuiz.id").as_str().unwrap().to_string();

    let res = gql(
        &router,
        Some(&b_tok),
        r#"query($id: ID!) { quiz(id: $id) { quiz { title } } }"#,
        json!({ "id": quiz_id }),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("FORBIDDEN"));

    // non-owner cannot update
    let res = gql(
        &router,
        Some(&b_tok),
        r#"mutation($id: ID!) { updateQuiz(id: $id, input: { title: "Hacked" }) { id } }"#,
        json!({ "id": quiz_id }),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("NOT_FOUND"));

    // attempts on private quiz forbidden for others
    let res = gql(
        &router,
        Some(&b_tok),
        r#"mutation($id: ID!) { startAttempt(quizId: $id) { id } }"#,
        json!({ "id": quiz_id }),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("FORBIDDEN"));

    // weak password rejected
    let res = gql(
        &router,
        None,
        r#"mutation { register(username: "weaky", password: "short") { token } }"#,
        json!({}),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("BAD_REQUEST"));
}

#[tokio::test]
async fn question_validation() {
    let (router, _s) = setup().await;
    let (tok, _) = register(&router, "validator").await;
    let res = gql(
        &router,
        Some(&tok),
        r#"mutation { createQuiz(input: { title: "V" }) { id } }"#,
        json!({}),
    )
    .await;
    let quiz_id = data(&res, "createQuiz.id").as_str().unwrap().to_string();

    // single choice with zero correct options
    let res = gql(
        &router,
        Some(&tok),
        r#"mutation($q: ID!) { addQuestions(quizId: $q, input: [
            { kind: SINGLE, text: "t", options: [ {text:"a", isCorrect:false}, {text:"b", isCorrect:false} ] }
        ]) { id } }"#,
        json!({ "q": quiz_id }),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("BAD_REQUEST"));

    // empty batch
    let res = gql(
        &router,
        Some(&tok),
        r#"mutation($q: ID!) { addQuestions(quizId: $q, input: []) { id } }"#,
        json!({ "q": quiz_id }),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("BAD_REQUEST"));
}

#[tokio::test]
async fn time_limit_and_max_attempts() {
    let (router, state) = setup().await;
    let (tok, _) = register(&router, "limituser").await;

    let res = gql(
        &router,
        Some(&tok),
        r#"mutation { createQuiz(input: { title: "Speed", timeLimitSec: 1, maxAttempts: 1 }) { id } }"#,
        json!({}),
    )
    .await;
    let quiz_id = data(&res, "createQuiz.id").as_str().unwrap().to_string();

    gql(
        &router,
        Some(&tok),
        r#"mutation($q: ID!) { addQuestions(quizId: $q, input: [
            { kind: BOOLEAN, text: "1+1=2?", options: [ {text:"True", isCorrect:true}, {text:"False", isCorrect:false} ] }
        ]) { id } }"#,
        json!({ "q": quiz_id }),
    )
    .await;

    let res = gql(
        &router,
        Some(&tok),
        r#"mutation($quiz: ID!) { startAttempt(quizId: $quiz) { id } }"#,
        json!({ "quiz": quiz_id }),
    )
    .await;
    let attempt_id = data(&res, "startAttempt.id").as_str().unwrap().to_string();
    let attempt_num: i64 = attempt_id.parse().unwrap();

    // simulate an expired attempt by backdating started_at
    sqlx::query("UPDATE attempts SET started_at = started_at - 60000 WHERE id = ?1")
        .bind(attempt_num)
        .execute(&state.pool)
        .await
        .unwrap();

    let res = gql(
        &router,
        Some(&tok),
        r#"mutation($a: ID!) { submitAttempt(attemptId: $a, answers: []) { score } }"#,
        json!({ "a": attempt_id }),
    )
    .await;
    assert_eq!(
        error_code(&res).as_deref(),
        Some("CONFLICT"),
        "expired attempt must be rejected: {res}"
    );

    // max_attempts=1 already consumed
    let res = gql(
        &router,
        Some(&tok),
        r#"mutation($quiz: ID!) { startAttempt(quizId: $quiz) { id } }"#,
        json!({ "quiz": quiz_id }),
    )
    .await;
    assert_eq!(error_code(&res).as_deref(), Some("CONFLICT"));
}

/// The hot path: many users submit attempts for the same quiz concurrently.
/// This exercises BEGIN CONCURRENT optimistic page-level locking plus the
/// retry engine under real contention.
#[tokio::test]
async fn concurrent_submits_all_commit() {
    const USERS: usize = 12;
    let (router, _state) = setup().await;
    let (author_tok, _) = register(&router, "benchauthor").await;

    let res = gql(
        &router,
        Some(&author_tok),
        r#"mutation { createQuiz(input: { title: "Concurrent" }) { id } }"#,
        json!({}),
    )
    .await;
    let quiz_id = data(&res, "createQuiz.id").as_str().unwrap().to_string();

    // 10 boolean questions, 1 pt each
    let mut input = String::from("[");
    for i in 0..10 {
        if i > 0 {
            input.push(',');
        }
        input.push_str(&format!(
            r#"{{ kind: BOOLEAN, text: "q{i}", options: [
                {{text: "True", isCorrect: true}}, {{text: "False", isCorrect: false}} ] }}"#
        ));
    }
    input.push(']');
    let addq =
        format!(r#"mutation($q: ID!) {{ addQuestions(quizId: $q, input: {input}) {{ id }} }}"#);
    let res = gql(&router, Some(&author_tok), &addq, json!({ "q": quiz_id })).await;
    assert!(res["errors"].is_null(), "seed questions failed: {res}");

    // register players
    let mut players = Vec::new();
    for i in 0..USERS {
        players.push(register(&router, &format!("cplayer{i}")).await);
    }

    // everyone starts an attempt first (read+write mix)
    let mut attempts = Vec::new();
    for (tok, _) in &players {
        let res = gql(
            &router,
            Some(tok),
            r#"mutation($quiz: ID!) { startAttempt(quizId: $quiz) { id } }"#,
            json!({ "quiz": quiz_id }),
        )
        .await;
        assert!(res["errors"].is_null(), "{res}");
        attempts.push(data(&res, "startAttempt.id").as_str().unwrap().to_string());
    }

    // fetch question ids as one player
    let res = gql(
        &router,
        Some(&players[0].0.clone()),
        r#"query($id: ID!) { quiz(id: $id) { questions { id options { id } } } }"#,
        json!({ "id": quiz_id }),
    )
    .await;
    let questions = data(&res, "quiz.questions").as_array().unwrap().clone();

    // all players submit at the same moment; user i answers i+1 questions
    // correctly (score = i+1)
    let mut handles = Vec::new();
    for (i, ((tok, _), attempt_id)) in players.iter().zip(attempts.iter()).enumerate() {
        let router = router.clone();
        let tok = tok.clone();
        let attempt = attempt_id.clone();
        let correct = (i + 1).min(questions.len());
        let qs = questions.clone();
        handles.push(tokio::spawn(async move {
            let mut answers = Vec::new();
            for (qi, q) in qs.iter().enumerate() {
                let opts = q["options"].as_array().unwrap();
                let pick = if qi < correct { 0 } else { 1 }; // True = correct
                answers.push(json!({
                    "questionId": q["id"],
                    "selectedOptionIds": [opts[pick]["id"]],
                }));
            }
            let res = gql(
                &router,
                Some(&tok),
                r#"mutation($a: ID!, $ans: [AnswerInput!]!) {
                    submitAttempt(attemptId: $a, answers: $ans) { score maxScore correctCount }
                }"#,
                json!({ "a": attempt, "ans": answers }),
            )
            .await;
            (i, res)
        }));
    }

    let mut scores = Vec::new();
    for h in handles {
        let (i, res) = h.await.unwrap();
        assert!(
            res["errors"].is_null(),
            "player {i} submit failed under contention: {res}"
        );
        let expected = ((i + 1).min(10)) as f64;
        let got = data(&res, "submitAttempt.score").as_f64().unwrap();
        assert_eq!(got, expected, "player {i} score mismatch");
        scores.push(got);
    }
    assert_eq!(scores.len(), USERS);

    // leaderboard must rank all players, best first
    let res = gql(
        &router,
        None,
        r#"query($q: ID!) { leaderboard(quizId: $q, first: 50) { rank score user { username } } }"#,
        json!({ "q": quiz_id }),
    )
    .await;
    let lb = data(&res, "leaderboard").as_array().unwrap().clone();
    assert_eq!(lb.len(), USERS);
    for w in lb.windows(2) {
        assert!(
            w[0]["score"].as_f64().unwrap() >= w[1]["score"].as_f64().unwrap(),
            "leaderboard not sorted"
        );
    }
    assert_eq!(lb[0]["rank"], json!(1));
}

#[tokio::test]
async fn health_and_metrics_and_playground() {
    let (router, _s) = setup().await;

    let (status, body) = get(&router, "/healthz").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let health: Value = serde_json::from_str(&body).unwrap();
    // Proof that the service runs OUR custom SQLite build.
    assert_eq!(health["sqlite"], json!("3.55.0"));
    assert_eq!(health["tx_mode"], json!("concurrent"));

    let (status, _body) = get(&router, "/readyz").await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // generate some traffic, then check metrics
    gql(&router, None, r#"{ quizzes { total } }"#, json!({})).await;
    let (status, body) = get(&router, "/metrics").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        body.contains("http_requests_total"),
        "missing http metric:\n{body}"
    );
    assert!(
        body.contains("graphql"),
        "metrics should be labeled by route:\n{body}"
    );

    let (status, body) = get(&router, "/playground").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body.contains("GraphQL Playground"));
}

#[tokio::test]
async fn sqlite_info_query_reports_custom_build() {
    let (router, _s) = setup().await;
    let res = gql(&router, None, r#"{ sqliteInfo }"#, json!({})).await;
    let info = data(&res, "sqliteInfo");
    let info = info.as_str().unwrap();
    assert!(info.starts_with("SQLite 3.55.0"), "unexpected: {info}");
    assert!(info.contains("begin-concurrent"));
}
