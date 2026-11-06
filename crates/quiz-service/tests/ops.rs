//! Retention with a movable clock, snapshots, and the internal listener.

mod common;

use axum::http::StatusCode;
use common::*;
use quiz_service::auth::Role;
use quiz_service::game::store::{DbStore, GameStore};
use quiz_service::repo::games::{AnswerWrite, Batch, NewGame, PlayerWrite};
use quiz_service::retention::{self, KEPT_QUIZ_TTL_MS, RESULTS_TTL_MS, UNKEPT_QUIZ_TTL_MS};
use quiz_service::server::{internal_router, public_router};
use quiz_service::snapshot::{Outcome, Snapshotter};
use quiz_service::state::AppState;
use quiz_service::telemetry;
use serde_json::{Value, json};
use std::time::Duration;

const DAY: i64 = 24 * 60 * 60 * 1000;

/// Times are read from the real clock plus the test's offset: allow the
/// seconds a slow test run takes.
fn near(a: i64, b: i64) {
    assert!((a - b).abs() < 5_000, "{a} is not near {b}");
}

async fn quiz_count(state: &AppState) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM quizzes")
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

async fn game_count(state: &AppState) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM games")
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

/// A finished game with two players and two answers, written the way the
/// engine writes them.
async fn play_game(state: &AppState, quiz: &Value, host: &str) -> i64 {
    let store = DbStore {
        state: state.clone(),
        spans_ready: state.spans_ready.clone(),
    };
    let quiz_id: i64 = quiz["id"].as_str().unwrap().parse().unwrap();
    let question_id: i64 = quiz["questions"][0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let game_id = quiz_service::ids::new_id();
    let now = state.now_ms();
    store
        .create_game(NewGame {
            id: game_id,
            quiz_id,
            session_id: "s1".into(),
            class_id: "c1".into(),
            host_id: host.into(),
            started_at: now,
            mode: 0,
        })
        .await
        .unwrap();
    let (p1, p2) = (quiz_service::ids::new_id(), quiz_service::ids::new_id());
    let player = |id, user: &str, score| PlayerWrite {
        id,
        user_id: Some(user.into()),
        name: format!("Name {user}"),
        joined_at: now,
        score,
        total_ms: 4000,
        correct_count: 1,
        answered: 1,
        best_streak: 1,
    };
    let answer = |player_id, points| AnswerWrite {
        id: quiz_service::ids::new_id(),
        player_id,
        question_id,
        selected: vec![1],
        correct: true,
        credit: 1.0,
        points,
        bonus: 0,
        time_ms: 4000,
    };
    store
        .write(Batch {
            game_id,
            players: vec![player(p1, "student1", 900), player(p2, "student2", 700)],
            answers: vec![answer(p1, 900), answer(p2, 700)],
            ended_at: Some(now),
        })
        .await
        .unwrap();
    game_id
}

#[tokio::test]
async fn kept_and_unkept_quizzes_expire_after_90_and_7_days_of_not_being_used() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let unkept = post(&router, "/v1/quizzes", &t, {
        let mut b = quiz_body("quick");
        b["kept"] = json!(false);
        b
    })
    .await
    .body;
    let kept = post(&router, "/v1/quizzes", &t, quiz_body("bank"))
        .await
        .body;
    let (u_id, k_id) = (unkept["id"].as_str().unwrap(), kept["id"].as_str().unwrap());

    assert_eq!(unkept["kept"], false);
    assert_eq!(
        unkept["expiresAt"].as_i64().unwrap() - unkept["createdAt"].as_i64().unwrap(),
        UNKEPT_QUIZ_TTL_MS
    );
    assert_eq!(
        kept["expiresAt"].as_i64().unwrap() - kept["createdAt"].as_i64().unwrap(),
        KEPT_QUIZ_TTL_MS
    );

    // nothing goes early
    state.clock.advance_ms(6 * DAY);
    assert_eq!(
        retention::sweep(&state, state.now_ms())
            .await
            .unwrap()
            .quizzes,
        0
    );

    // looking at a quiz is not a use: its expiry stays where it was
    let before = get(&router, &format!("/v1/quizzes/{u_id}"), &t).await.body["expiresAt"].clone();
    state.clock.advance_ms(DAY / 2);
    let after = get(&router, &format!("/v1/quizzes/{u_id}"), &t).await.body["expiresAt"].clone();
    assert_eq!(before, after, "viewing does not move the expiry");
    get(&router, "/v1/quizzes", &t).await;

    // an edit is a use: it moves the expiry a full period on
    put(
        &router,
        &format!("/v1/quizzes/{k_id}"),
        &t,
        quiz_body("bank, edited"),
    )
    .await;
    let moved = get(&router, &format!("/v1/quizzes/{k_id}"), &t).await.body["expiresAt"]
        .as_i64()
        .unwrap();
    near(moved, state.now_ms() + KEPT_QUIZ_TTL_MS);

    // day 8: the unkept quiz that was never used again is gone
    state.clock.advance_ms(2 * DAY);
    let swept = retention::sweep(&state, state.now_ms()).await.unwrap();
    assert_eq!(swept.quizzes, 1);
    assert_eq!(
        get(&router, &format!("/v1/quizzes/{u_id}"), &t)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&router, &format!("/v1/quizzes/{k_id}"), &t)
            .await
            .status,
        StatusCode::OK
    );

    // the kept one lives 90 days after its last use, not before
    state.clock.advance_ms(87 * DAY);
    assert_eq!(
        retention::sweep(&state, state.now_ms())
            .await
            .unwrap()
            .quizzes,
        0,
        "day 89 of its period"
    );
    state.clock.advance_ms(2 * DAY);
    assert_eq!(
        retention::sweep(&state, state.now_ms())
            .await
            .unwrap()
            .quizzes,
        1
    );
    assert_eq!(quiz_count(&state).await, 0);
}

#[tokio::test]
async fn keeping_switches_the_period_both_ways_and_starting_a_game_is_a_use() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let quiz = post(&router, "/v1/quizzes", &t, quiz_body("q")).await.body;
    let id = quiz["id"].as_str().unwrap();
    let uri = format!("/v1/quizzes/{id}/kept");

    state.clock.advance_ms(3 * DAY);
    let letgo = put(&router, &uri, &t, json!({ "kept": false })).await;
    assert_eq!(letgo.status, StatusCode::OK);
    assert_eq!(letgo.body["kept"], false);
    near(
        letgo.body["expiresAt"].as_i64().unwrap(),
        state.now_ms() + UNKEPT_QUIZ_TTL_MS,
    );

    state.clock.advance_ms(DAY);
    let keep = put(&router, &uri, &t, json!({ "kept": true })).await;
    near(
        keep.body["expiresAt"].as_i64().unwrap(),
        state.now_ms() + KEPT_QUIZ_TTL_MS,
    );

    // a game started on an unkept quiz moves its 7 days
    put(&router, &uri, &t, json!({ "kept": false })).await;
    let full = get(&router, &format!("/v1/quizzes/{id}"), &t).await.body;
    state.clock.advance_ms(5 * DAY);
    let store = DbStore {
        state: state.clone(),
        spans_ready: state.spans_ready.clone(),
    };
    store
        .create_game(NewGame {
            id: quiz_service::ids::new_id(),
            quiz_id: id.parse().unwrap(),
            session_id: "s".into(),
            class_id: "c".into(),
            host_id: "teacher1".into(),
            started_at: state.now_ms(),
            mode: 0,
        })
        .await
        .unwrap();
    let used = get(&router, &format!("/v1/quizzes/{id}"), &t).await.body;
    near(
        used["expiresAt"].as_i64().unwrap(),
        state.now_ms() + UNKEPT_QUIZ_TTL_MS,
    );
    assert!(used["expiresAt"].as_i64().unwrap() > full["expiresAt"].as_i64().unwrap());
}

#[tokio::test]
async fn results_expire_90_days_after_the_game_while_the_quiz_stays() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let quiz = post(&router, "/v1/quizzes", &t, quiz_body("q")).await.body;
    let id = quiz["id"].as_str().unwrap();
    let ended = state.now_ms();
    play_game(&state, &quiz, "teacher1").await;

    let games = get(&router, &format!("/v1/quizzes/{id}/games"), &t).await;
    assert_eq!(games.body[0]["players"][0]["name"], "Name student1");
    assert_eq!(games.body[0]["players"][0]["rank"], 1);
    assert_eq!(games.body[0]["questions"][0]["answeredCount"], 2);
    near(
        games.body[0]["expiresAt"].as_i64().unwrap(),
        ended + RESULTS_TTL_MS,
    );

    // day 80: the teacher uses the quiz again, so it stays past the results
    state.clock.advance_ms(80 * DAY);
    put(
        &router,
        &format!("/v1/quizzes/{id}"),
        &t,
        quiz_body("q again"),
    )
    .await;
    state.clock.advance_ms(11 * DAY);
    let swept = retention::sweep(&state, state.now_ms()).await.unwrap();
    assert_eq!((swept.quizzes, swept.games), (0, 1));
    assert_eq!(quiz_count(&state).await, 1, "the quiz stays");
    assert_eq!(
        get(&router, &format!("/v1/quizzes/{id}/games"), &t)
            .await
            .body,
        json!([])
    );
    let leftovers: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM players) + (SELECT COUNT(*) FROM answers)",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(leftovers, 0, "players and answers go with their game");
}

#[tokio::test]
async fn deleting_a_quiz_deletes_its_games_at_once() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let quiz = post(&router, "/v1/quizzes", &t, quiz_body("q")).await.body;
    play_game(&state, &quiz, "teacher1").await;
    assert_eq!(game_count(&state).await, 1);
    delete(
        &router,
        &format!("/v1/quizzes/{}", quiz["id"].as_str().unwrap()),
        &t,
    )
    .await;
    assert_eq!(game_count(&state).await, 0);
}

#[tokio::test]
async fn the_sweep_runs_at_startup_and_closes_games_the_last_process_left_running() {
    let state = setup().await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    let old = post(&router, "/v1/quizzes", &t, {
        let mut b = quiz_body("old");
        b["kept"] = json!(false);
        b
    })
    .await
    .body;
    let fresh = post(&router, "/v1/quizzes", &t, quiz_body("fresh"))
        .await
        .body;
    // a game that never ended (the service died mid-lesson)
    let store = DbStore {
        state: state.clone(),
        spans_ready: state.spans_ready.clone(),
    };
    store
        .create_game(NewGame {
            id: quiz_service::ids::new_id(),
            quiz_id: fresh["id"].as_str().unwrap().parse().unwrap(),
            session_id: "s".into(),
            class_id: "c".into(),
            host_id: "teacher1".into(),
            started_at: state.now_ms(),
            mode: 0,
        })
        .await
        .unwrap();
    state.clock.advance_ms(8 * DAY);

    retention::spawn(state.clone());
    for _ in 0..100 {
        if quiz_count(&state).await == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        quiz_count(&state).await,
        1,
        "the old unkept quiz went at startup"
    );
    assert_eq!(
        get(
            &router,
            &format!("/v1/quizzes/{}", old["id"].as_str().unwrap()),
            &t
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM games WHERE ended_at IS NULL")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(
        open, 0,
        "the orphaned game was closed and now has its own countdown"
    );
}

#[tokio::test]
async fn forgetting_a_person_deletes_their_quizzes_and_blanks_them_in_others_results() {
    let state = setup().await;
    let router = public_router(state.clone());
    let internal = internal_router(state.clone(), telemetry::init_metrics());
    let teacher = ticket(&state, Role::Teacher, "teacher1");
    let leaving = ticket(&state, Role::Teacher, "leaving-teacher");
    let mine = post(&router, "/v1/quizzes", &teacher, quiz_body("mine"))
        .await
        .body;
    post(&router, "/v1/quizzes", &leaving, quiz_body("theirs")).await;
    play_game(&state, &mine, "teacher1").await;

    let no_key = call(
        &internal,
        "POST",
        "/internal/users/student1/forget",
        None,
        None,
        &[],
    )
    .await;
    assert_eq!(no_key.status, StatusCode::UNAUTHORIZED);
    let wrong = call(
        &internal,
        "POST",
        "/internal/users/student1/forget",
        None,
        None,
        &[("x-internal-key", "wrong")],
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);

    let r = call(
        &internal,
        "POST",
        "/internal/users/student1/forget",
        None,
        None,
        &[("x-internal-key", INTERNAL_KEY)],
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, json!({ "quizzes": 0, "players": 1 }));
    let games = get(
        &router,
        &format!("/v1/quizzes/{}/games", mine["id"].as_str().unwrap()),
        &teacher,
    )
    .await
    .body;
    let names: Vec<_> = games[0]["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].clone(), p["anonymous"].clone()))
        .collect();
    assert!(names.contains(&(json!(""), json!(true))), "{names:?}");
    assert!(names.contains(&(json!("Name student2"), json!(false))));

    let r = call(
        &internal,
        "POST",
        "/internal/users/leaving-teacher/forget",
        None,
        None,
        &[("x-internal-key", INTERNAL_KEY)],
    )
    .await;
    assert_eq!(r.body, json!({ "quizzes": 1, "players": 0 }));
    assert_eq!(get(&router, "/v1/quizzes", &leaving).await.body["total"], 0);
    assert_eq!(
        get(&router, "/v1/quizzes", &teacher).await.body["total"],
        1,
        "others keep theirs"
    );
}

#[tokio::test]
async fn the_internal_listener_serves_probes_and_metrics() {
    let state = setup().await;
    let internal = internal_router(state, telemetry::init_metrics());
    for path in ["/healthz", "/readyz", "/metrics"] {
        let r = call(&internal, "GET", path, None, None, &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
    }
    let health = call(&internal, "GET", "/healthz", None, None, &[]).await;
    assert!(
        health.body["sqlite"].as_str().unwrap().starts_with("3.55"),
        "our custom SQLite"
    );
    for path in ["/v1/quizzes", "/ws"] {
        assert_eq!(
            call(&internal, "GET", path, None, None, &[]).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

/// The gauge's current value from the metrics text.
fn snapshot_gauge() -> f64 {
    telemetry::init_metrics()
        .render()
        .lines()
        .find_map(|l| l.strip_prefix("quiz_snapshot_last_success_seconds "))
        .expect("the gauge is exported")
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn snapshots_are_atomic_and_the_gauge_advances_even_when_nothing_changed() {
    telemetry::init_metrics();
    let dir = temp_path("snapshots");
    let state = setup_with(&[("QUIZ_SNAPSHOT_DIR", dir.to_string_lossy().into())]).await;
    let router = public_router(state.clone());
    let t = ticket(&state, Role::Teacher, "teacher1");
    post(&router, "/v1/quizzes", &t, quiz_body("snapshotted")).await;

    let mut snap = Snapshotter::new(dir.clone());
    assert_eq!(snap.run_once(&state).await.unwrap(), Outcome::Written);
    let file = dir.join("quiz.db");
    assert!(file.exists());
    assert!(
        !dir.join("quiz.db.tmp").exists(),
        "the temporary file is gone after the rename"
    );
    let first = snapshot_gauge();
    let written_at = std::fs::metadata(&file).unwrap().modified().unwrap();

    // the snapshot is a database of its own: open it and read the quiz back
    use sqlx::ConnectOptions;
    use sqlx::sqlite::SqliteConnectOptions;
    let mut conn = SqliteConnectOptions::new()
        .filename(&file)
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let title: String = sqlx::query_scalar("SELECT title FROM quizzes")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(title, "snapshotted");
    drop(conn);

    // an idle interval writes no file but the gauge still moves forward
    state.clock.advance_ms(15 * 60 * 1000);
    assert_eq!(snap.run_once(&state).await.unwrap(), Outcome::Unchanged);
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        written_at,
        "no new file"
    );
    let second = snapshot_gauge();
    assert!(
        second >= first + 15.0 * 60.0 - 1.0,
        "the gauge advanced by an interval: {first} -> {second}"
    );

    // a change is snapshotted at the next interval
    post(&router, "/v1/quizzes", &t, quiz_body("another")).await;
    state.clock.advance_ms(15 * 60 * 1000);
    assert_eq!(snap.run_once(&state).await.unwrap(), Outcome::Written);
    assert!(snapshot_gauge() >= second + 15.0 * 60.0 - 1.0);
}
