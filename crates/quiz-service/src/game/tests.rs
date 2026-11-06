//! Room and game tests: paused time, a fake store, fake sockets.

use super::frames::{ClientFrame, Out};
use super::registry::{Deps, Rooms};
use super::room::{Member, RoomHandle};
use super::store::{GameStore, LoadedQuiz};
use crate::auth::Role;
use crate::error::{AppError, AppResult};
use crate::grading::tests::question;
use crate::model::QuestionKind;
use crate::repo::games::{Batch, NewGame};
use crate::repo::spans::Span;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

const BASE: i64 = 1_800_000_000_000;
const ROOM_CLOSES: i64 = BASE + 2 * 60 * 60 * 1000;

#[derive(Default)]
struct FakeStore {
    quizzes: Mutex<HashMap<(i64, String), LoadedQuiz>>,
    games: Mutex<Vec<NewGame>>,
    batches: Mutex<Vec<Batch>>,
    spans: Mutex<Vec<Span>>,
    fail_writes: Mutex<bool>,
}

impl GameStore for FakeStore {
    fn load_quiz(&self, quiz_id: i64, owner_id: String) -> BoxFuture<'_, AppResult<LoadedQuiz>> {
        Box::pin(async move {
            self.quizzes
                .lock()
                .unwrap()
                .get(&(quiz_id, owner_id))
                .cloned()
                .ok_or_else(|| AppError::NotFound("quiz".into()))
        })
    }
    fn create_game(&self, game: NewGame) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            self.games.lock().unwrap().push(game);
            Ok(())
        })
    }
    fn write(&self, batch: Batch) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            if *self.fail_writes.lock().unwrap() {
                return Err(AppError::Internal("disk".into()));
            }
            self.batches.lock().unwrap().push(batch);
            Ok(())
        })
    }
    fn write_spans(&self, spans: Vec<Span>) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            self.spans.lock().unwrap().extend(spans);
            Ok(())
        })
    }
}

/// Quiz 1: single (right = option 2, bit 2), multi (right = options 1 and 3),
/// true/false (right = second). Limits 20 s, 20 s, 10 s.
fn quiz() -> LoadedQuiz {
    LoadedQuiz {
        id: 1,
        title: "Fractions".into(),
        questions: Arc::new(vec![
            question(
                10,
                QuestionKind::Single,
                20,
                &[(100, false), (101, true), (102, false), (103, false)],
            ),
            question(
                11,
                QuestionKind::Multi,
                20,
                &[(110, true), (111, false), (112, true), (113, false)],
            ),
            question(12, QuestionKind::Boolean, 10, &[(120, false), (121, true)]),
        ]),
    }
}

struct World {
    store: Arc<FakeStore>,
    deps: Deps,
}

fn world() -> World {
    let store = Arc::new(FakeStore::default());
    store
        .quizzes
        .lock()
        .unwrap()
        .insert((1, "teacher".into()), quiz());
    let deps = Deps {
        store: store.clone(),
        now_ms: Arc::new(|| BASE),
        rooms: Rooms::default(),
        track_spans: true,
    };
    World { store, deps }
}

struct Client {
    sock: u64,
    room: RoomHandle,
    rx: mpsc::Receiver<Out>,
}

impl Client {
    async fn send(&self, frame: ClientFrame) {
        self.room.frame(self.sock, frame).await;
    }

    /// Frames waiting now, as JSON.
    fn frames(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(o) = self.rx.try_recv() {
            if let Some(t) = o.into_text() {
                out.push(serde_json::from_str(&t).unwrap());
            }
        }
        out
    }
}

/// Move virtual time forward, then let every task run.
async fn adv(d: Duration) {
    tokio::time::advance(d).await;
    settle().await;
}

/// Let every task run: advances a millisecond of virtual time.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

async fn connect(w: &World, sid: &str, user: &str, role: Role) -> Client {
    try_connect(w, sid, user, role).await.expect("attached")
}

async fn try_connect(w: &World, sid: &str, user: &str, role: Role) -> AppResult<Client> {
    let (out, rx) = super::registry::out_channel();
    let sock = w.deps.rooms.next_sock_id();
    let member = Member {
        sock,
        user: user.into(),
        name: format!("name-{user}"),
        role,
        cid: "class".into(),
        rc: ROOM_CLOSES,
        out,
    };
    let room = w.deps.rooms.attach(&w.deps, sid, member).await?;
    settle().await;
    Ok(Client { sock, room, rx })
}

async fn host(w: &World) -> Client {
    connect(w, "s1", "teacher", Role::Host).await
}

async fn player(w: &World, n: usize) -> Client {
    connect(w, "s1", &format!("p{n}"), Role::Player).await
}

fn kind(frame: &Value) -> u64 {
    frame[0].as_u64().unwrap()
}

fn last_of(frames: &[Value], k: u64) -> Option<&Value> {
    frames.iter().rev().find(|f| kind(f) == k)
}

async fn answer(c: &Client, q: u32, mask: u32) {
    c.send(ClientFrame::Answer { q, mask }).await;
}

#[tokio::test(start_paused = true)]
async fn a_room_without_a_game_says_so() {
    let w = world();
    let mut p = player(&w, 1).await;
    assert_eq!(p.frames(), vec![json!([0, BASE])]);
}

#[tokio::test(start_paused = true)]
async fn a_whole_game_scores_ranks_and_ends() {
    let w = world();
    let mut p1 = player(&w, 1).await;
    let mut p2 = player(&w, 2).await;
    let mut h = host(&w).await;
    p1.frames();
    p2.frames();
    h.frames();

    h.send(ClientFrame::Start(1)).await;
    settle().await;
    // students already in the room see the lobby; so does the host
    for c in [&mut p1, &mut p2, &mut h] {
        let f = c.frames();
        let lobby = last_of(&f, 1).expect("lobby frame");
        assert_eq!(lobby[2], json!("Fractions"));
        assert_eq!(lobby[3], json!(3));
    }

    h.send(ClientFrame::Next).await;
    settle().await;
    let q1 = last_of(&p1.frames(), 2).cloned().expect("question");
    assert_eq!(q1[2], json!(1), "question number");
    assert_eq!(q1[3], json!(3), "question count");
    assert_eq!(
        q1[4].as_i64().unwrap() - q1[1].as_i64().unwrap(),
        20_000,
        "endsAt is the server's deadline"
    );
    assert_eq!(q1[5], json!(0), "single choice");
    assert_eq!(q1[7].as_array().unwrap().len(), 4);

    // p1 answers right after 5 s, p2 wrong after 10 s
    adv(Duration::from_secs(5)).await;
    answer(&p1, 1, 0b0010).await;
    settle().await;
    assert_eq!(
        p1.frames(),
        vec![json!([9])],
        "an answer is acknowledged by one tiny frame"
    );
    assert!(
        p2.frames().iter().all(|f| kind(f) != 9 && kind(f) != 4),
        "nothing per answer goes to anyone else"
    );
    adv(Duration::from_secs(5)).await;
    answer(&p2, 1, 0b0001).await;
    settle().await;

    // everyone answered: the question closed by itself, before its deadline
    let f1 = p1.frames();
    let reveal = last_of(&f1, 5).expect("reveal frame");
    assert_eq!(reveal[1], json!(1));
    assert_eq!(reveal[2], json!(0b0010), "correct options");
    assert_eq!(reveal[3], json!([1, 1, 0, 0]), "how many chose each option");
    // [5, q, mask, counts, explanation, top, score, rank, points, right, mine]
    assert_eq!(
        &reveal.as_array().unwrap()[6..],
        &[json!(875), json!(1), json!(875), json!(1), json!(2)]
    );
    let f2 = p2.frames();
    let reveal2 = last_of(&f2, 5).unwrap();
    assert_eq!(
        &reveal2.as_array().unwrap()[6..],
        &[json!(0), json!(2), json!(0), json!(0), json!(1)]
    );
    let fh = h.frames();
    assert_eq!(
        last_of(&fh, 5).unwrap().as_array().unwrap().len(),
        6,
        "the host's reveal has no personal part"
    );

    // one write for the question: both answers and both players
    {
        let batches = w.store.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].answers.len(), 2);
        assert_eq!(batches[0].players.len(), 2);
    }

    // question 2 (multiple choice): p1 picks one of the two right options
    h.send(ClientFrame::Next).await;
    settle().await;
    p1.frames();
    answer(&p1, 2, 0b0001).await;
    answer(&p2, 2, 0b0101).await;
    settle().await;
    let f1 = p1.frames();
    let reveal = last_of(&f1, 5).unwrap();
    // half credit at 0 s: 1000 x 0.5
    assert_eq!(reveal[8], json!(500), "partial credit");
    assert_eq!(reveal[9], json!(0), "not fully right");

    // question 3, nobody answers: the deadline closes it
    h.send(ClientFrame::Next).await;
    settle().await;
    adv(Duration::from_secs(10)).await;
    settle().await;
    assert!(
        last_of(&p1.frames(), 5).is_some(),
        "the deadline closed the question"
    );

    // podium, then the end
    h.send(ClientFrame::Next).await;
    settle().await;
    let f = p2.frames();
    let podium = last_of(&f, 6).expect("podium");
    assert_eq!(
        podium[1][0][0],
        json!("name-p1"),
        "the faster, better player is first"
    );
    let fh = h.frames();
    assert_eq!(
        last_of(&fh, 6).unwrap().as_array().unwrap().len(),
        3,
        "the host gets the full order"
    );
    h.send(ClientFrame::Next).await;
    settle().await;
    assert_eq!(last_of(&p1.frames(), 7).unwrap(), &json!([7, 0]));
    let batches = w.store.batches.lock().unwrap();
    assert!(
        batches.last().unwrap().ended_at.is_some(),
        "the last write closes the game"
    );
}

#[tokio::test(start_paused = true)]
async fn the_deadline_closes_a_question_and_the_host_can_close_early() {
    let w = world();
    let mut p = player(&w, 1).await;
    let mut h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    p.frames();
    adv(Duration::from_secs(19)).await;
    assert!(last_of(&p.frames(), 5).is_none(), "not yet");
    adv(Duration::from_secs(2)).await;
    assert!(last_of(&p.frames(), 5).is_some(), "closed at 20 s");

    h.send(ClientFrame::Next).await;
    settle().await;
    p.frames();
    h.send(ClientFrame::Next).await; // the host closes question 2 at once
    settle().await;
    assert!(last_of(&p.frames(), 5).is_some());
    h.frames();
}

#[tokio::test(start_paused = true)]
async fn a_late_joiner_watches_and_scores_from_the_next_question() {
    let w = world();
    let mut p1 = player(&w, 1).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    let mut late = player(&w, 9).await;
    let f = late.frames();
    assert!(
        last_of(&f, 2).is_some(),
        "they see the open question at once"
    );
    answer(&late, 1, 0b0010).await;
    settle().await;
    assert_eq!(
        late.frames(),
        vec![json!([8, 5, 4])],
        "but cannot answer it"
    );
    answer(&p1, 1, 0b0010).await;
    settle().await;
    // p1 was the only eligible player: the question closed
    let f = late.frames();
    let reveal = last_of(&f, 5).unwrap();
    assert_eq!(reveal[6], json!(0));
    h.send(ClientFrame::Next).await;
    settle().await;
    answer(&late, 2, 0b0101).await;
    settle().await;
    assert_eq!(
        last_of(&late.frames(), 9).map(|_| ()),
        Some(()),
        "acknowledged from question 2"
    );
    p1.frames();
}

#[tokio::test(start_paused = true)]
async fn a_reconnect_resumes_with_its_own_answer_and_score() {
    let w = world();
    let mut p = player(&w, 1).await;
    let q = player(&w, 2).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    p.frames();
    answer(&p, 1, 0b0100).await;
    settle().await;
    p.room.detach(p.sock).await;
    settle().await;
    // the question is still open (p2 has not answered); p1 comes back
    let mut again = player(&w, 1).await;
    let f = again.frames();
    assert_eq!(kind(&f[0]), 2, "the first frame is the current state");
    assert_eq!(
        f[0].as_array().unwrap().last().unwrap(),
        &json!(0b0100),
        "with the player's own answer"
    );
    answer(&again, 1, 0b0001).await;
    settle().await;
    assert_eq!(
        again.frames(),
        vec![json!([8, 5, 4])],
        "the first answer locked"
    );
    drop(q);
}

#[tokio::test(start_paused = true)]
async fn the_key_is_not_in_a_question_before_the_reveal() {
    let w = world();
    let mut p = player(&w, 1).await;
    let mut p2 = player(&w, 2).await;
    let mut h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    for c in [&mut p, &mut p2, &mut h] {
        for f in c.frames() {
            if kind(&f) == 2 {
                assert_eq!(
                    f.as_array().unwrap().len(),
                    8,
                    "no key, no points, nothing but the question"
                );
            }
            assert_ne!(kind(&f), 5, "no reveal yet");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn roles_may_only_do_their_own_part() {
    let w = world();
    let mut p = player(&w, 1).await;
    let mut h = host(&w).await;
    p.frames();
    p.send(ClientFrame::Start(1)).await;
    p.send(ClientFrame::Next).await;
    p.send(ClientFrame::End).await;
    settle().await;
    assert_eq!(
        p.frames(),
        vec![json!([8, 2, 1]), json!([8, 2, 2]), json!([8, 2, 3])]
    );
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    h.frames();
    h.send(ClientFrame::Answer { q: 1, mask: 1 }).await;
    settle().await;
    assert_eq!(h.frames(), vec![json!([8, 2, 4])], "the host does not play");

    assert!(matches!(
        try_connect(&w, "s1", "other", Role::Host).await,
        Err(AppError::Forbidden)
    ));
    assert!(matches!(
        try_connect(&w, "s1", "t", Role::Teacher).await,
        Err(AppError::Forbidden)
    ));
}

#[tokio::test(start_paused = true)]
async fn one_running_game_per_session_and_per_teacher() {
    let w = world();
    let mut h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    let f = h.frames();
    assert_eq!(
        last_of(&f, 8).unwrap(),
        &json!([8, 5, 1]),
        "a second game in the session"
    );
    let mut elsewhere = connect(&w, "s2", "teacher", Role::Host).await;
    elsewhere.send(ClientFrame::Start(1)).await;
    settle().await;
    assert_eq!(
        last_of(&elsewhere.frames(), 8).unwrap(),
        &json!([8, 5, 1]),
        "a second game of the teacher"
    );
    // an unknown quiz, or one that is not theirs
    let mut other = connect(&w, "s3", "someone", Role::Host).await;
    other.send(ClientFrame::Start(1)).await;
    settle().await;
    assert_eq!(last_of(&other.frames(), 8).unwrap(), &json!([8, 3, 1]));
    // the failed start did not leave the teacher blocked
    other.send(ClientFrame::Start(1)).await;
    settle().await;
    assert_eq!(last_of(&other.frames(), 8).unwrap(), &json!([8, 3, 1]));
}

#[tokio::test(start_paused = true)]
async fn the_hosts_count_is_refreshed_at_most_every_500_ms() {
    let w = world();
    let mut ps = Vec::new();
    for n in 0..10 {
        ps.push(player(&w, n).await);
    }
    let mut h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    h.frames();
    // ten answers within 100 ms (the last player never answers)
    for p in ps.iter().take(9) {
        answer(p, 1, 0b0010).await;
        adv(Duration::from_millis(10)).await;
    }
    let early = h.frames().into_iter().filter(|f| kind(f) == 4).count();
    assert_eq!(early, 1, "one at once, the rest held back");
    adv(Duration::from_millis(500)).await;
    let later = h
        .frames()
        .into_iter()
        .filter(|f| kind(f) == 4)
        .collect::<Vec<_>>();
    assert_eq!(later, vec![json!([4, 9, 10])], "then the latest count");
    for p in &mut ps {
        let f = p.frames();
        assert!(
            f.iter().all(|f| kind(f) != 4),
            "players never get the count"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_game_ends_at_the_room_close_and_after_an_idle_hour() {
    let w = world();
    let h = host(&w).await;
    let mut p = player(&w, 1).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    p.frames();
    adv(Duration::from_secs(60 * 60 + 1)).await;
    assert_eq!(
        last_of(&p.frames(), 7).unwrap(),
        &json!([7, 2]),
        "idle for an hour"
    );

    h.send(ClientFrame::Start(1)).await;
    settle().await;
    p.frames();
    // the host keeps the game alive with commands; the room still closes
    for _ in 0..3 {
        adv(Duration::from_secs(50 * 60)).await;
        h.send(ClientFrame::Next).await;
        settle().await;
    }
    let f = p.frames();
    assert_eq!(last_of(&f, 7).unwrap(), &json!([7, 1]), "the room closed");
}

#[tokio::test(start_paused = true)]
async fn a_failed_write_is_retried_with_the_next_one() {
    let w = world();
    let p = player(&w, 1).await;
    let mut h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    *w.store.fail_writes.lock().unwrap() = true;
    answer(&p, 1, 0b0010).await;
    settle().await;
    assert!(w.store.batches.lock().unwrap().is_empty());
    *w.store.fail_writes.lock().unwrap() = false;
    h.send(ClientFrame::End).await;
    settle().await;
    let batches = w.store.batches.lock().unwrap();
    assert_eq!(
        batches.last().unwrap().answers.len(),
        1,
        "the answer was kept for the final write"
    );
    h.frames();
}

#[tokio::test(start_paused = true)]
async fn forgetting_a_player_blanks_their_name_and_forgetting_the_host_ends_the_game() {
    let w = world();
    let mut p = player(&w, 1).await;
    let q = player(&w, 2).await;
    let mut h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    w.deps.rooms.forget("p1").await;
    settle().await;
    assert!(p.rx.try_recv().is_ok(), "their socket is told");
    answer(&q, 1, 0b0010).await;
    settle().await;
    h.send(ClientFrame::End).await;
    settle().await;
    {
        let batches = w.store.batches.lock().unwrap();
        let last_players: Vec<_> = batches.iter().flat_map(|b| b.players.iter()).collect();
        assert!(
            last_players
                .iter()
                .all(|p| p.user_id.as_deref() != Some("p1"))
        );
        assert!(
            last_players
                .iter()
                .any(|p| p.user_id.is_none() && p.name.is_empty())
        );
    }
    // a host that is forgotten ends their game
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    h.frames();
    w.deps.rooms.forget("teacher").await;
    settle().await;
    assert!(w.deps.rooms.running_quiz_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn at_most_500_players_and_a_slow_socket_is_cut() {
    let w = world();
    let mut h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    let mut ps = Vec::new();
    for n in 0..500 {
        ps.push(player(&w, n).await);
    }
    assert!(matches!(
        try_connect(&w, "s1", "p501", Role::Player).await,
        Err(AppError::Conflict(_))
    ));
    // a socket nobody reads from: its queue fills and it is cut
    for _ in 0..10 {
        h.send(ClientFrame::Next).await;
        h.send(ClientFrame::Next).await;
        settle().await;
        for p in ps.iter_mut().skip(1) {
            p.frames();
        }
        h.frames();
    }
    while ps[0].rx.try_recv().is_ok() {}
    assert!(
        matches!(
            ps[0].rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ),
        "the unread socket was cut"
    );
}

#[tokio::test(start_paused = true)]
async fn spans_follow_who_was_connected() {
    let w = world();
    let p = player(&w, 1).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    adv(Duration::from_secs(30)).await;
    let leaver = player(&w, 2).await;
    adv(Duration::from_secs(30)).await;
    leaver.room.detach(leaver.sock).await;
    settle().await;
    for _ in 0..5 {
        adv(Duration::from_secs(60)).await;
    }
    h.send(ClientFrame::End).await;
    settle().await;
    let spans = w.store.spans.lock().unwrap().clone();
    let closed: Vec<_> = spans.iter().filter(|s| s.closed).collect();
    assert_eq!(
        closed.len(),
        3,
        "host, p1 and p2 each have one closed span: {spans:?}"
    );
    let p2 = closed.iter().find(|s| s.user_id == "p2").unwrap();
    assert!(
        (30_000..30_100).contains(&(p2.to_ms - p2.from_ms)),
        "closed at the moment they left: {spans:?}"
    );
    let host_open = spans
        .iter()
        .filter(|s| !s.closed && s.role == "host")
        .count();
    assert!(
        host_open >= 4,
        "the host's open span was rewritten each minute"
    );
    drop(p);
}

#[tokio::test(start_paused = true)]
async fn not_answering_never_beats_a_wrong_answer_in_the_tie_break() {
    let w = world();
    let mut tried = player(&w, 1).await;
    let mut silent = player(&w, 2).await;
    let mut h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    adv(Duration::from_secs(1)).await;
    answer(&tried, 1, 0b0001).await; // wrong, after a second
    settle().await;
    h.send(ClientFrame::Next).await; // the host closes: the other never answered
    settle().await;
    let f = tried.frames();
    let reveal = last_of(&f, 5).unwrap();
    assert_eq!(
        reveal[7],
        json!(1),
        "the one who tried is ahead of the silent one"
    );
    let f = silent.frames();
    assert_eq!(last_of(&f, 5).unwrap()[7], json!(2));
    h.frames();
}
