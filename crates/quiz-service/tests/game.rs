//! The game over real sockets: a running service, websocket clients and a
//! fake Go server receiving lesson-time spans.

mod common;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post as route_post;
use common::*;
use futures::{SinkExt, StreamExt};
use quiz_service::auth::Role;
use quiz_service::game::store::{DbStore, GameStore};
use quiz_service::push::Pusher;
use quiz_service::repo::spans::{self, Span};
use quiz_service::server::public_router;
use quiz_service::state::AppState;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ------------------------------------------------------------ fake Go server

#[derive(Default)]
struct Fake {
    requests: Vec<Value>,
    keys: Vec<String>,
    /// Answers (status and body) to give before falling back to 204.
    script: VecDeque<(StatusCode, String)>,
    /// A request naming one of these sessions is answered 404 with the id the
    /// Go server uses for an unknown session.
    unknown_sessions: HashSet<String>,
}

type FakeState = Arc<Mutex<Fake>>;

async fn spans_route(
    State(fake): State<FakeState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> (StatusCode, String) {
    let mut f = fake.lock().unwrap();
    f.keys.push(
        headers
            .get("x-internal-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
    );
    let unknown = body["spans"].as_array().unwrap().iter().any(|s| {
        f.unknown_sessions
            .contains(s["session_id"].as_str().unwrap())
    });
    f.requests.push(body);
    if let Some(answer) = f.script.pop_front() {
        return answer;
    }
    if unknown {
        (StatusCode::NOT_FOUND, session_unknown_body())
    } else {
        (StatusCode::NO_CONTENT, String::new())
    }
}

fn session_unknown_body() -> String {
    json!({ "id": quiz_service::push::SESSION_UNKNOWN, "message": "no such session" }).to_string()
}

/// Make the fake answer the next request with this status and body.
fn answer_next(fake: &FakeState, status: StatusCode, body: &str) {
    fake.lock()
        .unwrap()
        .script
        .push_back((status, body.to_string()));
}

async fn fake_go() -> (String, FakeState) {
    let fake = FakeState::default();
    let app = Router::new()
        .route("/api/v4/lms/internal/quiz/spans", route_post(spans_route))
        .with_state(fake.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}"), fake)
}

// -------------------------------------------------------------- the service

struct Service {
    state: AppState,
    addr: SocketAddr,
    router: Router,
}

async fn serve(extra: &[(&str, String)]) -> Service {
    let state = setup_with(extra).await;
    let router = public_router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router.clone();
    tokio::spawn(async move { axum::serve(listener, app).await });
    Service {
        state,
        addr,
        router,
    }
}

impl Service {
    async fn quiz(&self, owner: &str) -> String {
        let t = ticket(&self.state, Role::Teacher, owner);
        post(&self.router, "/v1/quizzes", &t, quiz_body("Live quiz"))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn open(&self, token: &str) -> Ws {
        self.open_with(token, Some(SITE), true)
            .await
            .expect("upgrade")
    }

    async fn open_with(
        &self,
        _token: &str,
        origin: Option<&str>,
        protocol: bool,
    ) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
        let mut req = format!("ws://{}/ws", self.addr)
            .into_client_request()
            .unwrap();
        if let Some(o) = origin {
            req.headers_mut()
                .insert("origin", HeaderValue::from_str(o).unwrap());
        }
        if protocol {
            req.headers_mut().insert(
                "sec-websocket-protocol",
                HeaderValue::from_static("quiz.v1"),
            );
        }
        tokio_tungstenite::connect_async(req)
            .await
            .map(|(ws, _)| ws)
    }

    /// Connect, say hello, and return the socket with its first frame.
    async fn join(&self, token: &str) -> (Ws, Value) {
        let mut ws = self.open(token).await;
        send(&mut ws, json!([0, token])).await;
        let first = recv(&mut ws).await;
        (ws, first)
    }
}

async fn send(ws: &mut Ws, frame: Value) {
    ws.send(Message::text(frame.to_string())).await.unwrap();
}

/// The next text frame as JSON.
async fn recv(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(8), ws.next())
            .await
            .expect("a frame in time");
        match msg {
            Some(Ok(Message::Text(t))) => return serde_json::from_str(&t).unwrap(),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("expected a frame, got {other:?}"),
        }
    }
}

/// Frames until one of type `kind` arrives.
async fn recv_kind(ws: &mut Ws, kind: u64) -> Value {
    loop {
        let f = recv(ws).await;
        if f[0].as_u64() == Some(kind) {
            return f;
        }
    }
}

async fn closed(ws: &mut Ws) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => continue,
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                    _ => return false,
                }
            }
        })
        .await,
        Ok(true)
    )
}

fn host_ticket(s: &Service, sub: &str) -> String {
    ticket(&s.state, Role::Host, sub)
}

fn player_ticket(s: &Service, sub: &str) -> String {
    ticket(&s.state, Role::Player, sub)
}

// ------------------------------------------------------------------ the game

#[tokio::test]
async fn a_whole_game_over_real_sockets_is_scored_stored_and_billed_as_time() {
    let (lms, fake) = fake_go().await;
    let s = serve(&[("QUIZ_LMS_URL", lms)]).await;
    let quiz_id = s.quiz("teacher1").await;

    let (mut host, first) = s.join(&host_ticket(&s, "teacher1")).await;
    assert_eq!(first[0], 0, "no game yet");
    assert!(
        first[1].as_i64().unwrap() > 1_700_000_000_000,
        "serverNow, for clock correction"
    );
    let mut players = Vec::new();
    for n in 1..=3 {
        let (ws, first) = s.join(&player_ticket(&s, &format!("student{n}"))).await;
        assert_eq!(first[0], 0);
        players.push(ws);
    }

    send(&mut host, json!([1, quiz_id])).await;
    let lobby = recv_kind(&mut host, 1).await;
    assert_eq!(
        (lobby[2].clone(), lobby[3].clone()),
        (json!("Live quiz"), json!(3))
    );
    for p in &mut players {
        assert_eq!(
            recv_kind(p, 1).await[2],
            "Live quiz",
            "students already in the room see the game begin"
        );
    }

    // question 1: students 1 and 2 answer right, student 3 wrong
    send(&mut host, json!([2])).await;
    for p in &mut players {
        let q = recv_kind(p, 2).await;
        assert_eq!(q[2], 1);
        assert_eq!(q[7], json!(["1", "2", "0", "1/4"]));
    }
    send(&mut players[0], json!([4, 1, 1])).await;
    send(&mut players[1], json!([4, 1, 1])).await;
    send(&mut players[2], json!([4, 1, 2])).await;
    for p in &mut players {
        assert_eq!(recv_kind(p, 9).await, json!([9]));
    }
    let reveal = recv_kind(&mut players[0], 5).await;
    assert_eq!(reveal[2], 1, "the correct options as a mask");
    assert_eq!(reveal[3], json!([2, 1, 0, 0]));
    assert_eq!(reveal[4], "one whole");
    assert!(
        reveal[6].as_i64().unwrap() > 900,
        "answered at once: close to 1000 points"
    );

    // questions 2 and 3, then the podium
    for (q, mask) in [(2, 0b0101), (3, 0b01)] {
        send(&mut host, json!([2])).await;
        for p in &mut players {
            recv_kind(p, 2).await;
        }
        send(&mut players[0], json!([4, q, mask])).await;
        let _ = recv_kind(&mut players[0], 9).await;
        send(&mut host, json!([2])).await; // the host closes it
        recv_kind(&mut players[0], 5).await;
    }
    send(&mut host, json!([2])).await;
    let podium = recv_kind(&mut players[0], 6).await;
    assert_eq!(podium[1][0][0], "Name student1");
    let podium_host = recv_kind(&mut host, 6).await;
    assert_eq!(
        podium_host[2].as_array().unwrap().len(),
        3,
        "the host gets the whole order"
    );
    send(&mut host, json!([3])).await;
    assert_eq!(recv_kind(&mut players[0], 7).await, json!([7, 0]));

    // the results are in the bank, the spans in the outbox
    let t = ticket(&s.state, Role::Teacher, "teacher1");
    let games = get(&s.router, &format!("/v1/quizzes/{quiz_id}/games"), &t)
        .await
        .body;
    assert_eq!(games[0]["players"].as_array().unwrap().len(), 3);
    assert_eq!(games[0]["players"][0]["name"], "Name student1");
    assert_eq!(games[0]["players"][0]["correctCount"], 3);
    assert_eq!(games[0]["questions"][0]["answeredCount"], 3);
    assert_eq!(games[0]["questions"][0]["correctCount"], 2);
    let answers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM answers")
        .fetch_one(&s.state.pool)
        .await
        .unwrap();
    assert_eq!(
        answers, 5,
        "three answers to question 1, one each to 2 and 3"
    );

    let pusher = Pusher::new(&s.state).unwrap();
    assert!(pusher.drain(false).await);
    let sent: Vec<Value> = fake
        .lock()
        .unwrap()
        .requests
        .iter()
        .flat_map(|r| r["spans"].as_array().unwrap().clone())
        .collect();
    let users: HashSet<&str> = sent
        .iter()
        .map(|x| x["user_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        users,
        HashSet::from(["teacher1", "student1", "student2", "student3"])
    );
    let mine = sent.iter().find(|x| x["user_id"] == "teacher1").unwrap();
    assert_eq!(mine["role"], "host");
    assert_eq!(mine["session_id"], "s1");
    assert!(mine["game_id"].as_str().unwrap().parse::<i64>().is_ok());
    assert!(mine["to"].as_i64().unwrap() >= mine["from"].as_i64().unwrap());
    assert_eq!(fake.lock().unwrap().keys[0], INTERNAL_KEY);
    assert_eq!(
        spans::depth(&s.state.pool).await.unwrap(),
        0,
        "acknowledged spans leave the outbox"
    );
}

#[tokio::test]
async fn a_reconnect_resumes_the_game_and_extends_the_same_span() {
    let (lms, fake) = fake_go().await;
    let s = serve(&[("QUIZ_LMS_URL", lms)]).await;
    let quiz_id = s.quiz("teacher1").await;
    let (mut host, _) = s.join(&host_ticket(&s, "teacher1")).await;
    let ptoken = player_ticket(&s, "student1");
    let (mut p, _) = s.join(&ptoken).await;
    let (_other, _) = s.join(&player_ticket(&s, "student2")).await; // never answers: the question stays open
    send(&mut host, json!([1, quiz_id])).await;
    recv_kind(&mut p, 1).await;
    send(&mut host, json!([2])).await;
    recv_kind(&mut p, 2).await;
    send(&mut p, json!([4, 1, 1])).await;
    recv_kind(&mut p, 9).await;
    drop(p);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (mut again, first) = s.join(&ptoken).await;
    assert_eq!(first[0], 2, "the first frame is the question as it stands");
    assert_eq!(
        first.as_array().unwrap().last().unwrap(),
        &json!(1),
        "with the answer already given"
    );
    send(&mut again, json!([4, 1, 2])).await;
    assert_eq!(
        recv(&mut again).await,
        json!([8, 5, 4]),
        "the first answer locked"
    );
    send(&mut host, json!([3])).await;
    recv_kind(&mut again, 7).await;

    let pusher = Pusher::new(&s.state).unwrap();
    assert!(pusher.drain(false).await);
    let sent: Vec<Value> = fake
        .lock()
        .unwrap()
        .requests
        .iter()
        .flat_map(|r| r["spans"].as_array().unwrap().clone())
        .collect();
    assert_eq!(
        sent.iter().filter(|x| x["user_id"] == "student1").count(),
        1,
        "one span, not two"
    );
}

#[tokio::test]
async fn the_socket_checks_origin_protocol_hello_and_ticket() {
    let s = serve(&[]).await;
    let ok = player_ticket(&s, "student1");

    for (name, origin, protocol) in [
        ("another origin", Some("https://evil.example"), true),
        ("no origin", None, true),
        ("no subprotocol", Some(SITE), false),
    ] {
        let r = s.open_with(&ok, origin, protocol).await;
        assert!(r.is_err(), "{name} must not upgrade");
    }

    // a bad ticket, a teacher's ticket, and a ticket for no session
    let mut ws = s.open(&ok).await;
    send(&mut ws, json!([0, "garbage"])).await;
    assert_eq!(recv(&mut ws).await, json!([8, 1, 0]));
    assert!(closed(&mut ws).await);

    let teacher = ticket(&s.state, Role::Teacher, "t");
    let mut ws = s.open(&teacher).await;
    send(&mut ws, json!([0, teacher])).await;
    assert_eq!(recv(&mut ws).await, json!([8, 2, 0]));
    assert!(closed(&mut ws).await);

    // something other than hello first
    let mut ws = s.open(&ok).await;
    send(&mut ws, json!([2])).await;
    assert_eq!(recv(&mut ws).await, json!([8, 4, 0]));
    assert!(closed(&mut ws).await);

    // saying nothing for 5 s
    let mut ws = s.open(&ok).await;
    let started = std::time::Instant::now();
    assert_eq!(recv(&mut ws).await, json!([8, 1, 0]));
    assert!(closed(&mut ws).await);
    assert!(
        started.elapsed() >= Duration::from_secs(5) && started.elapsed() < Duration::from_secs(7)
    );
}

#[tokio::test]
async fn sockets_per_person_frame_size_and_rate_are_limited() {
    let s = serve(&[]).await;
    let token = player_ticket(&s, "student1");
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(s.join(&token).await.0);
    }
    let mut fifth = s.open(&token).await;
    send(&mut fifth, json!([0, token])).await;
    assert_eq!(recv(&mut fifth).await, json!([8, 6, 0]));
    assert!(closed(&mut fifth).await);

    // after hello, a long frame closes the socket
    let (mut ws, _) = held.swap_remove(0).into_pair();
    ws.send(Message::text(format!("[4,1,{}]", "1".repeat(200))))
        .await
        .unwrap();
    assert!(closed(&mut ws).await);

    // and a flood is cut off
    let (mut ws, _) = s.join(&token).await.into_pair();
    for _ in 0..40 {
        if ws.send(Message::text("[4,9,1]")).await.is_err() {
            break;
        }
    }
    let mut cut = false;
    for _ in 0..80 {
        match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) if t.as_str() == "[8,6,0]" => cut = true,
            Ok(None | Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => break,
            _ => {}
        }
    }
    assert!(cut, "the flood was answered with a rate error");
}

trait IntoPair {
    fn into_pair(self) -> (Ws, ());
}

impl IntoPair for Ws {
    fn into_pair(self) -> (Ws, ()) {
        (self, ())
    }
}

impl IntoPair for (Ws, Value) {
    fn into_pair(self) -> (Ws, ()) {
        (self.0, ())
    }
}

// ------------------------------------------------------------------- spans

fn span(user: &str, session: &str, from: i64, to: i64, closed: bool) -> Span {
    Span {
        game_id: 77,
        user_id: user.into(),
        session_id: session.into(),
        role: "player".into(),
        from_ms: from,
        to_ms: to,
        closed,
    }
}

fn store(state: &AppState) -> DbStore {
    DbStore {
        state: state.clone(),
        spans_ready: state.spans_ready.clone(),
    }
}

fn sent(fake: &FakeState) -> Vec<Value> {
    fake.lock()
        .unwrap()
        .requests
        .iter()
        .flat_map(|r| r["spans"].as_array().unwrap().clone())
        .collect()
}

#[tokio::test]
async fn spans_are_pushed_in_order_in_batches_of_500_and_idempotently() {
    let (lms, fake) = fake_go().await;
    let state = setup_with(&[("QUIZ_LMS_URL", lms)]).await;
    let list: Vec<Span> = (0..1200)
        .map(|i| span(&format!("u{i:04}"), "s1", 1_000 + i, 2_000 + i, true))
        .collect();
    store(&state).write_spans(list.clone()).await.unwrap();
    // writing the same spans again changes nothing
    store(&state).write_spans(list).await.unwrap();
    assert_eq!(spans::depth(&state.pool).await.unwrap(), 1200);

    let pusher = Pusher::new(&state).unwrap();
    assert!(pusher.drain(false).await);
    let sizes: Vec<usize> = fake
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|r| r["spans"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, vec![500, 500, 200]);
    let froms: Vec<i64> = sent(&fake)
        .iter()
        .map(|x| x["from"].as_i64().unwrap())
        .collect();
    assert!(froms.windows(2).all(|w| w[0] <= w[1]), "oldest first");
    assert_eq!(spans::depth(&state.pool).await.unwrap(), 0);

    // nothing left to send: no request at all
    assert!(pusher.drain(false).await);
    assert_eq!(fake.lock().unwrap().requests.len(), 3);
}

#[tokio::test]
async fn a_hosts_open_span_is_resent_with_a_later_end_and_leaves_when_closed() {
    let (lms, fake) = fake_go().await;
    let state = setup_with(&[("QUIZ_LMS_URL", lms)]).await;
    let pusher = Pusher::new(&state).unwrap();
    let st = store(&state);

    st.write_spans(vec![span("h", "s1", 1_000, 61_000, false)])
        .await
        .unwrap();
    pusher.drain(false).await;
    assert_eq!(
        spans::depth(&state.pool).await.unwrap(),
        1,
        "an open span stays"
    );
    pusher.drain(false).await;
    assert_eq!(
        fake.lock().unwrap().requests.len(),
        1,
        "nothing new to say yet"
    );

    st.write_spans(vec![span("h", "s1", 1_000, 121_000, false)])
        .await
        .unwrap();
    pusher.drain(false).await;
    // an older end arriving late never shrinks it
    st.write_spans(vec![span("h", "s1", 1_000, 90_000, false)])
        .await
        .unwrap();
    pusher.drain(false).await;
    assert_eq!(fake.lock().unwrap().requests.len(), 2);
    let tos: Vec<i64> = sent(&fake)
        .iter()
        .map(|x| x["to"].as_i64().unwrap())
        .collect();
    assert_eq!(tos, vec![61_000, 121_000], "the end only grows");

    st.write_spans(vec![span("h", "s1", 1_000, 130_000, true)])
        .await
        .unwrap();
    pusher.drain(false).await;
    assert_eq!(
        spans::depth(&state.pool).await.unwrap(),
        0,
        "closed and acknowledged: gone"
    );
}

#[tokio::test]
async fn spans_are_retried_after_a_503_and_dropped_only_for_an_unknown_session() {
    quiz_service::telemetry::init_metrics();
    let (lms, fake) = fake_go().await;
    let state = setup_with(&[("QUIZ_LMS_URL", lms)]).await;
    let pusher = Pusher::new(&state).unwrap();
    let st = store(&state);

    // 503: kept, and sent again once the server recovers
    answer_next(&fake, StatusCode::SERVICE_UNAVAILABLE, "");
    st.write_spans(vec![span("a", "s1", 1, 2, true)])
        .await
        .unwrap();
    assert!(
        !pusher.drain(false).await,
        "a failing server is not success"
    );
    assert_eq!(
        spans::depth(&state.pool).await.unwrap(),
        1,
        "still in the outbox"
    );
    let started = std::time::Instant::now();
    answer_next(&fake, StatusCode::BAD_GATEWAY, "");
    assert!(
        pusher.drain(true).await,
        "with retry it waits (backoff) and gets through"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "it backed off before retrying"
    );
    assert_eq!(spans::depth(&state.pool).await.unwrap(), 0);

    // 404 for an unknown session: that span is dropped, the others in the
    // same batch still get through
    fake.lock().unwrap().unknown_sessions.insert("ghost".into());
    st.write_spans(vec![
        span("b", "s1", 10, 20, true),
        span("c", "ghost", 11, 21, true),
        span("d", "s2", 12, 22, true),
    ])
    .await
    .unwrap();
    assert!(pusher.drain(true).await);
    assert_eq!(
        spans::depth(&state.pool).await.unwrap(),
        0,
        "the refused span did not stay"
    );
    let accepted: HashSet<String> = sent(&fake)
        .iter()
        .map(|x| x["user_id"].as_str().unwrap().to_string())
        .collect();
    assert!(accepted.contains("b") && accepted.contains("d"));
    let metrics = quiz_service::telemetry::init_metrics().render();
    assert!(metrics.contains("quiz_spans_dropped_total"), "{metrics}");
    assert!(metrics.contains("quiz_spans_pushed_total"));
    assert!(metrics.contains("quiz_spans_outbox_depth"));
}

#[tokio::test]
async fn a_refusal_that_may_pass_never_loses_a_span() {
    let (lms, fake) = fake_go().await;
    let state = setup_with(&[("QUIZ_LMS_URL", lms)]).await;
    let pusher = Pusher::new(&state).unwrap();
    let st = store(&state);
    st.write_spans(vec![
        span("a", "s1", 1, 2, true),
        span("b", "s1", 3, 4, true),
    ])
    .await
    .unwrap();

    // 401 (the feature is off or the keys differ), 403, 429 (rate limited), an
    // old server with no route (404 without the error id), 408, 5xx: the outbox
    // is untouched
    for (status, body) in [
        (StatusCode::UNAUTHORIZED, ""),
        (StatusCode::FORBIDDEN, ""),
        (StatusCode::TOO_MANY_REQUESTS, ""),
        (StatusCode::NOT_FOUND, ""),
        (
            StatusCode::NOT_FOUND,
            r#"{"id":"api.context.404.app_error"}"#,
        ),
        (StatusCode::REQUEST_TIMEOUT, ""),
        (StatusCode::INTERNAL_SERVER_ERROR, ""),
    ] {
        answer_next(&fake, status, body);
        assert!(!pusher.drain(false).await, "{status} is not success");
        assert_eq!(
            spans::depth(&state.pool).await.unwrap(),
            2,
            "{status} kept the spans"
        );
    }
    assert_eq!(
        fake.lock().unwrap().requests.len(),
        7,
        "each one was one whole batch, not split"
    );

    // a 429 is waited out with backoff and then goes through
    answer_next(&fake, StatusCode::TOO_MANY_REQUESTS, "");
    let started = std::time::Instant::now();
    assert!(pusher.drain(true).await);
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "it backed off"
    );
    assert_eq!(spans::depth(&state.pool).await.unwrap(), 0);
}

#[tokio::test]
async fn a_malformed_batch_is_dropped_and_the_span_of_an_unknown_session_alone() {
    let (lms, fake) = fake_go().await;
    let state = setup_with(&[("QUIZ_LMS_URL", lms)]).await;
    let pusher = Pusher::new(&state).unwrap();
    let st = store(&state);
    // 400, 413 and 422 mean the server will never take this
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::PAYLOAD_TOO_LARGE,
        StatusCode::UNPROCESSABLE_ENTITY,
    ] {
        st.write_spans(vec![span("x", "s1", 1, 2, true)])
            .await
            .unwrap();
        fake.lock()
            .unwrap()
            .script
            .push_back((status, String::new()));
        assert!(pusher.drain(false).await, "{status}");
        assert_eq!(
            spans::depth(&state.pool).await.unwrap(),
            0,
            "{status} dropped it"
        );
    }
    // a batch with one span of an unknown session: only that one goes
    fake.lock().unwrap().unknown_sessions.insert("ghost".into());
    st.write_spans(vec![
        span("good1", "s1", 10, 20, true),
        span("bad", "ghost", 11, 21, true),
        span("good2", "s1", 12, 22, true),
    ])
    .await
    .unwrap();
    assert!(pusher.drain(false).await);
    assert_eq!(spans::depth(&state.pool).await.unwrap(), 0);
    let accepted: HashSet<String> = sent(&fake)
        .iter()
        .map(|x| x["user_id"].as_str().unwrap().to_string())
        .collect();
    assert!(accepted.contains("good1") && accepted.contains("good2"));
}

#[tokio::test]
async fn shutdown_closes_every_open_span_and_makes_one_last_push() {
    let (lms, fake) = fake_go().await;
    let s = serve(&[("QUIZ_LMS_URL", lms)]).await;
    let quiz_id = s.quiz("teacher1").await;
    let (mut host, _) = s.join(&host_ticket(&s, "teacher1")).await;
    let (mut p, _) = s.join(&player_ticket(&s, "student1")).await;
    send(&mut host, json!([1, quiz_id])).await;
    recv_kind(&mut p, 1).await;
    send(&mut host, json!([2])).await;
    recv_kind(&mut p, 2).await;

    s.state.rooms.shutdown(Duration::from_secs(5)).await;
    assert_eq!(
        recv_kind(&mut p, 7).await,
        json!([7, 3]),
        "the game ended because the service restarts"
    );
    let pusher = Pusher::new(&s.state).unwrap();
    pusher.flush_once().await;
    let users: HashSet<String> = sent(&fake)
        .iter()
        .map(|x| x["user_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        users,
        HashSet::from(["teacher1".to_string(), "student1".to_string()])
    );
    let ended: Option<i64> = sqlx::query_scalar("SELECT ended_at FROM games")
        .fetch_one(&s.state.pool)
        .await
        .unwrap();
    assert!(ended.is_some(), "the game was closed in the database");
}

#[tokio::test]
async fn one_running_game_per_teacher_across_sessions() {
    let s = serve(&[]).await;
    let quiz_id = s.quiz("teacher1").await;
    let t1 = ticket_for(&s.state, Role::Host, "teacher1", "s1");
    let t2 = ticket_for(&s.state, Role::Host, "teacher1", "s2");
    let (mut a, _) = s.join(&t1).await;
    let (mut b, _) = s.join(&t2).await;
    send(&mut a, json!([1, quiz_id])).await;
    recv_kind(&mut a, 1).await;
    send(&mut b, json!([1, quiz_id])).await;
    assert_eq!(recv(&mut b).await, json!([8, 5, 1]));

    // a quiz with a running game cannot be edited or deleted
    let t = ticket(&s.state, Role::Teacher, "teacher1");
    assert_eq!(
        put(
            &s.router,
            &format!("/v1/quizzes/{quiz_id}"),
            &t,
            quiz_body("x")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        delete(&s.router, &format!("/v1/quizzes/{quiz_id}"), &t)
            .await
            .status,
        StatusCode::CONFLICT
    );
    send(&mut a, json!([3])).await;
    recv_kind(&mut a, 7).await;
    assert_eq!(
        delete(&s.router, &format!("/v1/quizzes/{quiz_id}"), &t)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn the_host_sees_names_and_selections_and_players_see_neither() {
    let s = serve(&[]).await;
    let quiz_id = s.quiz("teacher1").await;
    let (mut host, _) = s.join(&host_ticket(&s, "teacher1")).await;
    let (mut a, _) = s.join(&player_ticket(&s, "student1")).await;
    let (mut b, _) = s.join(&player_ticket(&s, "student2")).await;

    send(&mut host, json!([1, quiz_id])).await;
    let names = recv_kind(&mut host, 10).await;
    let mut listed: Vec<String> = names[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_string())
        .collect();
    let mut more = Vec::new();
    // the lobby may report the two names in one frame or two
    while listed.len() + more.len() < 2 {
        let f = recv_kind(&mut host, 10).await;
        more.extend(
            f[1].as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap().to_string()),
        );
    }
    listed.extend(more);
    listed.sort();
    assert_eq!(listed, vec!["Name student1", "Name student2"]);

    send(&mut host, json!([2])).await;
    recv_kind(&mut a, 2).await;
    recv_kind(&mut b, 2).await;
    send(&mut a, json!([4, 1, 1])).await; // right
    let progress = recv_kind(&mut host, 4).await;
    assert_eq!(
        progress.as_array().unwrap()[..4],
        [json!(4), json!(1), json!(2), json!([1, 0, 0, 0])],
        "one player chose option 1"
    );
    let ranking = progress[4].as_array().unwrap();
    assert_eq!(ranking.len(), 2);
    assert_eq!(
        ranking[0][0], "Name student1",
        "the player who answered leads"
    );
    assert!(ranking[0][1].as_i64().unwrap() > 900);
    assert_eq!(ranking[1][1], 0);

    // the players got an ack and the reveal, nothing about names or counts
    send(&mut b, json!([4, 1, 2])).await;
    for p in [&mut a, &mut b] {
        loop {
            let f = recv(p).await;
            assert!(f[0] != 4 && f[0] != 10, "players were sent {f}");
            if f[0] == 5 {
                break;
            }
        }
    }
    send(&mut host, json!([3])).await;
}

#[tokio::test]
async fn a_quest_game_over_real_sockets_from_start_to_stored_results() {
    let s = serve(&[]).await;
    let quiz_id = s.quiz("teacher1").await;
    let (mut host, _) = s.join(&host_ticket(&s, "teacher1")).await;
    let (mut a, _) = s.join(&player_ticket(&s, "student1")).await;
    let (mut b, _) = s.join(&player_ticket(&s, "student2")).await;

    // a quest of 5 minutes, friendly
    send(&mut host, json!([1, quiz_id, 1, 300, 1])).await;
    let lobby = recv_kind(&mut a, 20).await;
    assert_eq!(lobby[5], 300);
    assert_eq!(lobby[6], 1, "friendly");
    send(&mut a, json!([5, 4])).await; // a character
    let roster = recv_kind(&mut host, 21).await;
    assert!(!roster[1].as_array().unwrap().is_empty());

    send(&mut host, json!([2])).await;
    let running = recv_kind(&mut a, 22).await;
    assert_eq!(
        running[2].as_i64().unwrap() - running[1].as_i64().unwrap(),
        300_000
    );
    let q = recv_kind(&mut a, 24).await;
    let (kind, serial) = (q[1].as_u64().unwrap(), q[5].as_u64().unwrap());
    assert_eq!(
        q.as_array().unwrap().len(),
        7,
        "kind, text, options, limit, serial, time left"
    );
    // the quiz is single, multiple, true/false in order; the question text is
    // the authored one, whatever the order this player got
    let text = q[2].as_str().unwrap().to_string();
    let mask = match text.as_str() {
        "1/2 + 1/2 = ?" | "0.5 = 1/2" => 1,
        "Which are prime?" => 0b0101,
        other => panic!("unexpected question {other}"),
    };
    assert!(kind <= 2);
    send(&mut a, json!([6, serial, mask])).await;
    let verdict = recv_kind(&mut a, 25).await;
    assert_eq!(verdict[2], 1, "right");
    assert!(verdict[3].as_i64().unwrap() >= 10);
    send(&mut a, json!([7, serial, 0])).await;
    let chest = recv_kind(&mut a, 26).await;
    assert_eq!(
        chest[6].as_array().unwrap().len(),
        6,
        "the three outcomes are shown"
    );
    let next = recv_kind(&mut a, 24).await;
    assert_eq!(next[5], 2, "the next question is serial 2");

    // the other player never saw a's verdict or chest, and a wrong answer
    // costs a pause
    let q2 = recv_kind(&mut b, 24).await;
    send(&mut b, json!([6, q2[5], 0b1000_0000u32])).await; // an impossible choice
    assert_eq!(recv(&mut b).await, json!([8, 4, 6]));

    send(&mut host, json!([3])).await;
    assert_eq!(recv_kind(&mut a, 7).await, json!([7, 0]));

    // the results are in the bank, as a quest
    let t = ticket(&s.state, Role::Teacher, "teacher1");
    let games = get(&s.router, &format!("/v1/quizzes/{quiz_id}/games"), &t)
        .await
        .body;
    assert_eq!(games[0]["mode"], "quest");
    let players = games[0]["players"].as_array().unwrap();
    assert_eq!(players.len(), 2);
    assert_eq!(players[0]["name"], "Name student1");
    assert!(players[0]["score"].as_i64().unwrap() >= 10, "coins");
    assert_eq!(players[0]["answeredCount"], 1);
    assert_eq!(players[0]["correctCount"], 1);
    assert_eq!(players[0]["bestStreak"], 1);
    assert_eq!(games[0]["questions"], json!([]));
}
