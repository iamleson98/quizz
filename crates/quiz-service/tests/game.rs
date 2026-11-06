//! The game over real sockets: a running service and websocket clients.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::*;
use futures::{SinkExt, StreamExt};
use quiz_service::auth::Role;
use quiz_service::game::registry::Counts;
use quiz_service::server::public_router;
use quiz_service::state::AppState;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
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
        token: &str,
        origin: Option<&str>,
        protocol: bool,
    ) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
        self.open_from(token, origin, protocol, &[]).await
    }

    /// Like `open_with`, with extra request headers (the edge's client address).
    async fn open_from(
        &self,
        _token: &str,
        origin: Option<&str>,
        protocol: bool,
        headers: &[(&'static str, &str)],
    ) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
        let mut req = format!("ws://{}/ws", self.addr)
            .into_client_request()
            .unwrap();
        for (k, v) in headers {
            req.headers_mut()
                .insert(*k, HeaderValue::from_str(v).unwrap());
        }
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

/// The code and reason of the close frame that ends the socket, skipping
/// whatever text is still queued ahead of it.
async fn close_of(ws: &mut Ws) -> Option<(u16, String)> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(Some(frame)))) => {
                    return Some((frame.code.into(), frame.reason.to_string()));
                }
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return None,
            }
        }
    })
    .await
    .unwrap_or(None)
}

/// Wait until the service holds nothing for anybody: no room, no person's
/// slot, no address waiting for a hello, no game.
async fn settled(s: &Service) {
    let end = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let c = s.state.rooms.counts();
        if c == Counts::default() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "still holding something: {c:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn host_ticket(s: &Service, sub: &str) -> String {
    ticket(&s.state, Role::Host, sub)
}

fn player_ticket(s: &Service, sub: &str) -> String {
    ticket(&s.state, Role::Player, sub)
}

// ------------------------------------------------------------------ the game

#[tokio::test]
async fn a_whole_game_over_real_sockets_is_scored_and_stored() {
    let s = serve(&[]).await;
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

    // the results are in the bank
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
}

#[tokio::test]
async fn a_reconnect_resumes_the_game() {
    let s = serve(&[]).await;
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
    send(&mut host, json!([3])).await; // the podium first, then the end
    recv_kind(&mut again, 7).await;
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

    // saying nothing for 2 s
    let mut ws = s.open(&ok).await;
    let started = std::time::Instant::now();
    assert_eq!(recv(&mut ws).await, json!([8, 1, 0]));
    assert!(closed(&mut ws).await);
    assert!(
        started.elapsed() >= Duration::from_secs(2) && started.elapsed() < Duration::from_secs(4)
    );
}

#[tokio::test]
async fn a_fifth_socket_evicts_the_oldest_and_frame_size_and_rate_are_limited() {
    let s = serve(&[]).await;
    let token = player_ticket(&s, "student1");
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(s.join(&token).await.0);
    }
    // a fifth socket is welcome: the oldest is the one that goes (a phone that
    // lost its network must not lock the person out)
    let (fifth, first) = s.join(&token).await;
    assert_eq!(first[0], 0);
    assert!(closed(&mut held[0]).await, "the oldest was pushed out");
    held.remove(0);
    held.push(fifth);

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

#[tokio::test]
async fn shutdown_ends_a_running_game_and_closes_it_in_the_database() {
    let s = serve(&[]).await;
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
    assert_eq!(close_of(&mut p).await, Some((1001, "restart".into())));
    assert_eq!(close_of(&mut host).await, Some((1001, "restart".into())));
    settled(&s).await;
    let ended: Option<i64> = sqlx::query_scalar("SELECT ended_at FROM games")
        .fetch_one(&s.state.pool)
        .await
        .unwrap();
    assert!(ended.is_some(), "the game was closed in the database");
}

#[tokio::test]
async fn a_hello_after_the_lesson_ended_is_refused_but_a_late_one_within_the_grace_is_not() {
    let s = serve(&[]).await;
    let late = |ms_ago: i64, role| {
        ticket_closing(&s.state, role, "student1", "s1", s.state.now_ms() - ms_ago)
    };
    // the room closed 21 s ago: past the grace
    let (mut ws, first) = s.join(&late(21_000, Role::Player)).await;
    assert_eq!(first, json!([8, 1, 0]), "unauthorized, as for a bad ticket");
    assert!(closed(&mut ws).await);
    let (mut ws, first) = s.join(&late(3_600_000, Role::Host)).await;
    assert_eq!(first, json!([8, 1, 0]));
    assert!(closed(&mut ws).await);
    settled(&s).await;

    // closed 10 s ago: the grace lets the person see how it ended
    let (_ws, first) = s.join(&late(10_000, Role::Player)).await;
    assert_eq!(first[0], 0);
    // a teacher's bank ticket has no room close at all
    let teacher = ticket(&s.state, Role::Teacher, "teacher1");
    let (_ws, first) = s.join(&teacher).await;
    assert_eq!(
        first,
        json!([8, 2, 0]),
        "refused for its role, not its time"
    );
}

#[tokio::test]
async fn every_way_a_socket_ends_releases_its_slots_and_the_room() {
    let s = serve(&[
        ("QUIZ_WS_PING_SECS", "1".into()),
        ("QUIZ_WS_PONG_SECS", "1".into()),
    ])
    .await;
    let quiz_id = s.quiz("teacher1").await;

    // a socket that never says hello holds an address slot until it goes
    let silent = s.open(&player_ticket(&s, "x")).await;
    assert_eq!(s.state.rooms.counts().unknown_sockets, 1);
    drop(silent);
    settled(&s).await;
    // a bad ticket
    let (mut bad, first) = s.join("not-a-ticket").await;
    assert_eq!(first, json!([8, 1, 0]));
    assert!(closed(&mut bad).await);
    settled(&s).await;

    // the client closes: its person, its place in the room, the room
    let token = player_ticket(&s, "student1");
    let (mut ws, _) = s.join(&token).await;
    let c = s.state.rooms.counts();
    assert_eq!((c.rooms, c.people, c.sockets), (1, 1, 1));
    ws.close(None).await.unwrap();
    drop(ws);
    settled(&s).await;

    // five sockets of one person: the oldest is pushed out and its slot is
    // not counted twice
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(s.join(&token).await.0);
    }
    assert!(closed(&mut held[0]).await);
    let c = s.state.rooms.counts();
    assert_eq!((c.people, c.sockets), (1, 4), "{c:?}");
    drop(held);
    settled(&s).await;

    // a peer that stops answering pings
    let (mut quiet, _) = s.join(&token).await;
    tokio::time::sleep(Duration::from_millis(3200)).await;
    assert!(closed(&mut quiet).await, "no pong: closed");
    drop(quiet);
    settled(&s).await;

    // a game that ended, and then the last socket leaves
    let (mut host, _) = s.join(&host_ticket(&s, "teacher1")).await;
    let (mut p, _) = s.join(&token).await;
    send(&mut host, json!([1, quiz_id])).await;
    recv_kind(&mut p, 1).await;
    assert_eq!(s.state.rooms.counts().running_games, 1);
    send(&mut host, json!([3])).await;
    assert_eq!(recv_kind(&mut p, 7).await, json!([7, 0]));
    assert_eq!(s.state.rooms.counts().running_games, 0);
    assert_eq!(
        s.state.rooms.counts().rooms,
        1,
        "the sockets are still here"
    );
    drop(host);
    drop(p);
    settled(&s).await;
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
    // nobody answers in under 800 ms
    tokio::time::sleep(Duration::from_millis(900)).await;
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
    send(&mut host, json!([3])).await; // the podium first, then the end
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

#[tokio::test]
async fn sockets_that_have_not_said_hello_are_capped_per_client_address() {
    let s = serve(&[]).await;
    let ok = player_ticket(&s, "student1");
    // one school (one address) opens 64 sockets and says nothing
    let mut waiting = Vec::new();
    for _ in 0..64 {
        waiting.push(
            s.open_from(&ok, Some(SITE), true, &[("x-real-ip", "203.0.113.7")])
                .await
                .expect("within the cap"),
        );
    }
    let over = s
        .open_from(&ok, Some(SITE), true, &[("x-real-ip", "203.0.113.7")])
        .await;
    assert!(
        over.is_err(),
        "the 65th from that address is turned away (429)"
    );
    // another address is not locked out, and says hello normally
    let (ws, first) = {
        let mut ws = s
            .open_from(&ok, Some(SITE), true, &[("x-real-ip", "198.51.100.9")])
            .await
            .expect("another address");
        send(&mut ws, json!([0, ok])).await;
        let first = recv(&mut ws).await;
        (ws, first)
    };
    assert_eq!(first[0], 0);
    // Cloudflare's header wins over x-real-ip: the same school behind a
    // different edge header is the same count
    let behind_cf = s
        .open_from(
            &ok,
            Some(SITE),
            true,
            &[
                ("cf-connecting-ip", "203.0.113.7"),
                ("x-real-ip", "198.51.100.9"),
            ],
        )
        .await;
    assert!(
        behind_cf.is_err(),
        "cf-connecting-ip is the address that counts"
    );
    // a socket that says hello (or goes away) frees its slot
    drop(waiting.pop());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        s.open_from(&ok, Some(SITE), true, &[("x-real-ip", "203.0.113.7")])
            .await
            .is_ok()
    );
    drop(ws);
}

#[tokio::test]
async fn a_peer_that_never_answers_pings_is_dropped() {
    let s = serve(&[
        ("QUIZ_WS_PING_SECS", "1".into()),
        ("QUIZ_WS_PONG_SECS", "1".into()),
    ])
    .await;
    let token = player_ticket(&s, "student1");
    let (mut ws, _) = s.join(&token).await;
    // the client does not read, so tungstenite never answers the ping; after
    // the ping (1 s) and the pong deadline (1 s) the server closes
    tokio::time::sleep(Duration::from_millis(3200)).await;
    assert!(closed(&mut ws).await, "no pong: closed");

    // a peer that reads (and so answers) stays
    let (mut live, _) = s.join(&token).await;
    let end = tokio::time::Instant::now() + Duration::from_millis(3200);
    while let Ok(msg) = tokio::time::timeout_at(end, live.next()).await {
        assert!(
            matches!(msg, Some(Ok(Message::Ping(_)))),
            "a live socket is only pinged, never closed: {msg:?}"
        );
    }
}

#[tokio::test]
async fn the_biggest_host_frames_fit_a_socket() {
    use axum::routing::get;
    // the socket is tuned by `tune`, as /ws is; send it the worst frame there
    // is: a roster or a full order of 500 players with 160-byte names
    let names: Vec<String> = (0..500)
        .map(|i| format!("{i:03}{}", "\u{1F600}".repeat(39)))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let roster = quiz_service::game::frames::names(&refs);
    let podium = quiz_service::game::frames::close(quiz_service::game::frames::podium_head(
        &refs.iter().take(3).map(|n| (*n, 1)).collect::<Vec<_>>(),
        Some(&refs.iter().map(|n| (*n, 1)).collect::<Vec<_>>()),
    ));
    assert!(
        roster.len() > 80_000 && podium.len() > 80_000,
        "{} {}",
        roster.len(),
        podium.len()
    );
    let frames = vec![roster, podium];
    let app = Router::new().route(
        "/big",
        get(move |ws: axum::extract::ws::WebSocketUpgrade| {
            let frames = frames.clone();
            async move {
                quiz_service::ws::tune(ws).on_upgrade(move |mut socket| async move {
                    for f in frames {
                        socket
                            .send(axum::extract::ws::Message::Text(f))
                            .await
                            .unwrap();
                    }
                })
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let mut req = format!("ws://{addr}/big").into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("quiz.v1"),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    for _ in 0..2 {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => assert!(t.len() > 80_000),
            other => panic!("the big frame did not arrive: {other:?}"),
        }
    }
}
