//! Quest mode: paused time, a fake store and a scripted random source.

use super::chest::{Draw, Outcome, SeededDraw, roll_for};
use super::{ALLOWED_SECONDS, MIN_THINK_MS, PAUSE_MS, STANDINGS_EVERY};
use crate::auth::Role;
use crate::game::frames::ClientFrame;
use crate::game::room::{EMPTY_ROOM_LIMIT, HOST_AWAY_LIMIT, ROOM_CLOSE_GRACE};
use crate::game::store::LoadedQuiz;
use crate::game::tests::{
    BASE, CLOSED, Client, World, adv, connect, connect_rc, ended_reason, kind, last_of, leave,
    nothing_left, secs, settle, world_with,
};
use crate::grading::tests::question;
use crate::model::QuestionKind;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// Questions 20 to 23: single (right: first option), multiple choice (first
/// and third), true/false (second), single (fourth). Limits 20, 20, 10, 30 s.
fn quest_quiz() -> LoadedQuiz {
    let mut q20 = question(
        20,
        QuestionKind::Single,
        20,
        &[(200, true), (201, false), (202, false)],
    );
    q20.question.explanation = Some("because twenty".into());
    LoadedQuiz {
        id: 2,
        title: "Quest quiz".into(),
        questions: Arc::new(vec![
            q20,
            question(
                21,
                QuestionKind::Multi,
                20,
                &[(210, true), (211, false), (212, true), (213, false)],
            ),
            question(22, QuestionKind::Boolean, 10, &[(220, false), (221, true)]),
            question(
                23,
                QuestionKind::Single,
                30,
                &[(230, false), (231, false), (232, false), (233, true)],
            ),
        ]),
    }
}

/// The bit mask of the right options of question `id`.
fn right_mask(id: i64) -> u32 {
    match id {
        20 => 0b001,
        21 => 0b101,
        22 => 0b10,
        23 => 0b1000,
        _ => panic!("no question {id}"),
    }
}

fn limit_of(id: i64) -> i64 {
    match id {
        20 | 21 => 20,
        22 => 10,
        _ => 30,
    }
}

/// A random source whose `below` answers can be scripted by a test; the
/// rest comes from a seeded generator.
struct Scripted {
    script: Arc<Mutex<VecDeque<u64>>>,
    fallback: SeededDraw,
}

impl Draw for Scripted {
    fn next_u64(&mut self) -> u64 {
        self.fallback.next_u64()
    }

    fn below(&mut self, n: u64) -> u64 {
        match self.script.lock().unwrap().pop_front() {
            Some(v) => {
                assert!(v < n, "scripted {v} is not below {n}");
                v
            }
            None => self.fallback.below(n),
        }
    }
}

struct Quest {
    w: World,
    script: Arc<Mutex<VecDeque<u64>>>,
}

impl Quest {
    fn new() -> Quest {
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let s = script.clone();
        let w = world_with(Arc::new(move || {
            Box::new(Scripted {
                script: s.clone(),
                fallback: SeededDraw(11),
            })
        }));
        w.store
            .quizzes
            .lock()
            .unwrap()
            .insert((2, "teacher".into()), quest_quiz());
        Quest { w, script }
    }

    /// Make the next right answer's chests hold exactly these outcomes.
    fn chests(&self, streak_after_answer: u32, outcomes: [Outcome; 3]) {
        let mut s = self.script.lock().unwrap();
        for o in outcomes {
            s.push_back(roll_for(streak_after_answer, o));
        }
    }

    fn script_is_used_up(&self) {
        assert!(
            self.script.lock().unwrap().is_empty(),
            "scripted rolls left over"
        );
    }

    async fn player(&self, n: usize) -> Client {
        connect(&self.w, "s1", &format!("p{n}"), Role::Player).await
    }

    async fn host(&self) -> Client {
        connect(&self.w, "s1", "teacher", Role::Host).await
    }
}

async fn start(c: &Client, seconds: u32, flags: u32) {
    c.send(ClientFrame::StartQuest {
        quiz_id: 2,
        seconds,
        flags,
    })
    .await;
}

/// A question as served to one player.
#[derive(Debug, Clone, PartialEq)]
struct Served {
    id: i64,
    serial: u32,
    limit_sec: i64,
    remaining_ms: i64,
    kind: i64,
}

fn served(frames: &[Value]) -> Option<Served> {
    let f = last_of(frames, 24)?;
    let text = f[2].as_str().unwrap();
    Some(Served {
        id: text.trim_start_matches('q').parse().unwrap(),
        kind: f[1].as_i64().unwrap(),
        limit_sec: f[4].as_i64().unwrap(),
        serial: f[5].as_u64().unwrap() as u32,
        remaining_ms: f[6].as_i64().unwrap(),
    })
}

/// Think for a moment (an answer sooner than 800 ms after its question is
/// refused), then answer.
async fn reply(c: &Client, s: &Served, mask: u32) {
    adv(Duration::from_millis(MIN_THINK_MS + 50)).await;
    c.send(ClientFrame::QuestAnswer {
        serial: s.serial,
        mask,
    })
    .await;
}

fn wrong_mask(id: i64) -> u32 {
    if right_mask(id) == 1 { 0b10 } else { 0b1 }
}

/// Lobby, then running: returns the host and the players with the first
/// question of each already read.
async fn running(q: &Quest, n: usize, flags: u32) -> (Client, Vec<Client>, Vec<Served>) {
    let mut ps = Vec::new();
    for i in 1..=n {
        ps.push(q.player(i).await);
    }
    let mut h = q.host().await;
    start(&h, 300, flags).await;
    settle().await;
    adv(Duration::from_millis(600)).await;
    h.frames();
    for p in &mut ps {
        p.frames();
    }
    h.send(ClientFrame::Next).await;
    settle().await;
    let mut firsts = Vec::new();
    for p in &mut ps {
        let f = p.frames();
        firsts.push(served(&f).expect("a first question"));
    }
    (h, ps, firsts)
}

/// The verdict frame (type 25), if any.
fn verdict(frames: &[Value]) -> Option<Value> {
    last_of(frames, 25).cloned()
}

fn chest_result(frames: &[Value]) -> Option<Value> {
    last_of(frames, 26).cloned()
}

#[tokio::test(start_paused = true)]
async fn a_quest_opens_a_lobby_with_characters_and_the_hosts_roster() {
    let q = Quest::new();
    let mut p1 = q.player(1).await;
    let mut p2 = q.player(2).await;
    let mut h = q.host().await;
    p1.frames();
    p2.frames();
    h.frames();

    // only a length of 5, 10 or 15 minutes starts a game
    start(&h, 123, 0).await;
    settle().await;
    assert_eq!(h.frames(), vec![json!([8, 4, 1])]);
    assert_eq!(ALLOWED_SECONDS, [300, 600, 900]);

    start(&h, 600, 1).await;
    settle().await;
    for c in [&mut p1, &mut p2, &mut h] {
        let f = c.frames();
        let lobby = last_of(&f, 20).expect("the quest lobby");
        assert_eq!(
            (
                lobby[2].clone(),
                lobby[3].clone(),
                lobby[4].clone(),
                lobby[5].clone(),
                lobby[6].clone()
            ),
            (
                json!("Quest quiz"),
                json!(4),
                json!(2),
                json!(600),
                json!(1)
            ),
            "title, questions, players, seconds, friendly flag"
        );
    }

    // the host learns who is there, with their characters, and who changes
    adv(Duration::from_millis(600)).await;
    p1.send(ClientFrame::Character(7)).await;
    settle().await;
    adv(Duration::from_millis(600)).await;
    let f = h.frames();
    let rosters: Vec<_> = f.iter().filter(|f| kind(f) == 21).collect();
    assert!(
        rosters.iter().any(|r| r[1] == json!([["name-p1", 7]])),
        "the change of character reaches the host: {rosters:?}"
    );
    p2.send(ClientFrame::Character(12)).await;
    settle().await;
    assert_eq!(last_of(&p2.frames(), 8).unwrap(), &json!([8, 4, 5]));
    assert!(h.frames().iter().all(|f| kind(f) != 21));

    // players are never sent the roster
    for c in [&mut p1, &mut p2] {
        assert!(c.frames().iter().all(|f| kind(f) != 21));
    }
    // a host that says hello again gets the whole roster
    let mut again = q.host().await;
    let f = again.frames();
    let all = last_of(&f, 21).unwrap();
    assert_eq!(all[1].as_array().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_game_starts_for_everyone_with_their_own_queue() {
    let q = Quest::new();
    let (mut h, mut ps, firsts) = running(&q, 8, 0).await;
    assert!(firsts.iter().all(|s| s.serial == 1));
    for s in &firsts {
        assert_eq!(
            s.remaining_ms,
            limit_of(s.id) * 1000,
            "the full thinking time"
        );
    }
    let distinct: std::collections::HashSet<i64> = firsts.iter().map(|s| s.id).collect();
    assert!(
        distinct.len() > 1,
        "queues are shuffled per player: {firsts:?}"
    );
    // everybody learned the game is running and for how long
    let f = h.frames();
    let run = last_of(&f, 22).expect("running");
    assert_eq!(run[2].as_i64().unwrap() - run[1].as_i64().unwrap(), 300_000);
    // the host does not get a question
    assert!(f.iter().all(|f| kind(f) != 24));
    for p in &mut ps {
        assert!(p.frames().is_empty(), "nothing else yet");
    }
    // a second `next` while it runs is refused
    h.send(ClientFrame::Next).await;
    settle().await;
    assert_eq!(h.frames(), vec![json!([8, 5, 2])]);
}

#[tokio::test(start_paused = true)]
async fn the_queue_reshuffles_when_it_runs_out_and_never_repeats_the_last_question() {
    let q = Quest::new();
    let (_h, mut ps, firsts) = running(&q, 1, 0).await;
    let p = &mut ps[0];
    let mut ids = vec![firsts[0].id];
    let mut current = firsts[0].clone();
    for answered in 1..=11u32 {
        q.chests(answered, [Outcome::Coins(20); 3]);
        reply(p, &current, right_mask(current.id)).await;
        settle().await;
        q.script_is_used_up();
        p.send(ClientFrame::QuestPick {
            serial: current.serial,
            chest: 0,
        })
        .await;
        settle().await;
        let f = p.frames();
        current = served(&f).expect("the next question comes after the pick");
        assert_eq!(
            current.serial,
            answered + 1,
            "serials count the questions served"
        );
        ids.push(current.id);
    }
    for round in ids.chunks(4).filter(|c| c.len() == 4) {
        let mut sorted = round.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![20, 21, 22, 23],
            "each round asks every question once: {ids:?}"
        );
    }
    assert_ne!(
        ids[3], ids[4],
        "a new round does not start with the last question"
    );
    assert_ne!(ids[7], ids[8]);
}

#[tokio::test(start_paused = true)]
async fn right_answers_earn_coins_with_speed_and_a_streak_bonus_capped_at_25() {
    let q = Quest::new();
    let (_h, mut ps, firsts) = running(&q, 1, 0).await;
    let p = &mut ps[0];
    let mut current = firsts[0].clone();
    let mut total = 0;
    for (n, bonus) in [
        (1u32, 0i64),
        (2, 5),
        (3, 10),
        (4, 15),
        (5, 20),
        (6, 25),
        (7, 25),
    ] {
        q.chests(n, [Outcome::Coins(20); 3]);
        reply(p, &current, right_mask(current.id)).await;
        settle().await;
        let f = p.frames();
        let v = verdict(&f).expect("a verdict");
        assert_eq!(v[2], json!(1), "right");
        let earned = v[3].as_i64().unwrap();
        // 10 for being right, up to 10 for speed (answered at once: 9 or 10),
        // and the streak bonus
        assert!(
            earned == 10 + 9 + bonus || earned == 10 + 10 + bonus,
            "answer {n}: earned {earned}, bonus {bonus}"
        );
        assert_eq!(
            (v[4].clone(), v[5].clone()),
            (json!(n), json!(bonus)),
            "streak and bonus"
        );
        total += earned;
        assert_eq!(v[6].as_i64().unwrap(), total, "the running total");
        p.send(ClientFrame::QuestPick {
            serial: current.serial,
            chest: 1,
        })
        .await;
        settle().await;
        let f = p.frames();
        total += 20;
        assert_eq!(chest_result(&f).unwrap()[5].as_i64().unwrap(), total);
        current = served(&f).unwrap();
    }
    q.script_is_used_up();
}

#[tokio::test(start_paused = true)]
async fn the_speed_part_follows_the_remaining_thinking_time() {
    let q = Quest::new();
    let (_h, mut ps, firsts) = running(&q, 1, 0).await;
    let p = &mut ps[0];
    let s = firsts[0].clone();
    q.chests(1, [Outcome::Coins(20); 3]);
    adv(Duration::from_secs(10)).await; // half of a 20 s question; 1/3 of 30 s
    reply(p, &s, right_mask(s.id)).await;
    settle().await;
    let v = verdict(&p.frames()).unwrap();
    let limit = limit_of(s.id) * 1000;
    let speed = 10 * (limit - 10_000) / limit;
    let earned = v[3].as_i64().unwrap();
    assert!(
        (10 + speed - 1..=10 + speed).contains(&earned),
        "{earned} vs {}",
        10 + speed
    );
}

#[tokio::test(start_paused = true)]
async fn a_wrong_answer_shows_the_key_and_costs_a_pause_the_server_enforces() {
    let q = Quest::new();
    let (_h, mut ps, firsts) = running(&q, 1, 0).await;
    let p = &mut ps[0];
    let s = firsts[0].clone();
    reply(p, &s, wrong_mask(s.id)).await;
    settle().await;
    let f = p.frames();
    let v = verdict(&f).unwrap();
    assert_eq!(v[2], json!(0), "wrong");
    assert_eq!(v[7], json!(right_mask(s.id)), "the right options");
    assert_eq!(v[8], json!(PAUSE_MS));
    if s.id == 20 {
        assert_eq!(v[9], json!("because twenty"), "the explanation");
    }
    assert!(served(&f).is_none(), "no new question yet");

    // asking early is refused, before and just before the pause is over
    p.send(ClientFrame::QuestNext).await;
    settle().await;
    assert_eq!(p.frames(), vec![json!([8, 5, 8])]);
    adv(Duration::from_millis(PAUSE_MS - 200)).await;
    p.send(ClientFrame::QuestNext).await;
    settle().await;
    let f: Vec<Value> = p.frames().into_iter().filter(|f| kind(f) != 28).collect();
    assert_eq!(f, vec![json!([8, 5, 8])], "still thinking");
    // then the next question arrives by itself
    adv(Duration::from_millis(400)).await;
    let next = served(&p.frames()).expect("the next question");
    assert_eq!(next.serial, 2);
}

#[tokio::test(start_paused = true)]
async fn a_missed_question_counts_as_wrong_and_a_late_answer_is_refused() {
    let q = Quest::new();
    let (_h, mut ps, firsts) = running(&q, 1, 0).await;
    let p = &mut ps[0];
    let s = firsts[0].clone();
    adv(Duration::from_millis(limit_of(s.id) as u64 * 1000 - 100)).await;
    assert!(verdict(&p.frames()).is_none(), "still thinking");
    adv(Duration::from_millis(300)).await;
    let v = verdict(&p.frames()).expect("a verdict for the missed question");
    assert_eq!(v[2], json!(2), "missed");
    assert_eq!(v[7], json!(right_mask(s.id)));
    // the answer comes too late
    reply(p, &s, right_mask(s.id)).await;
    settle().await;
    assert_eq!(p.frames(), vec![json!([8, 5, 6])]);
    adv(Duration::from_millis(PAUSE_MS)).await;
    assert_eq!(served(&p.frames()).unwrap().serial, 2);
}

#[tokio::test(start_paused = true)]
async fn each_chest_outcome_does_what_it_says() {
    let q = Quest::new();
    let (_h, mut ps, firsts) = running(&q, 2, 0).await;
    let p = &mut ps[0];
    let mut cur = firsts[0].clone();

    // coins and the jackpot
    for (n, outcome, gain) in [
        (1u32, Outcome::Coins(20), 20),
        (2, Outcome::Coins(40), 40),
        (3, Outcome::Coins(80), 80),
        (4, Outcome::Jackpot, 200),
    ] {
        q.chests(n, [Outcome::Slip, outcome, Outcome::Slip]);
        reply(p, &cur, right_mask(cur.id)).await;
        settle().await;
        let coins = verdict(&p.frames()).unwrap()[6].as_i64().unwrap();
        p.send(ClientFrame::QuestPick {
            serial: cur.serial,
            chest: 1,
        })
        .await;
        settle().await;
        let f = p.frames();
        let c = chest_result(&f).unwrap();
        assert_eq!(c[2], json!(1), "the chest picked");
        assert_eq!(c[5].as_i64().unwrap(), coins + gain);
        let shown = c[6].as_array().unwrap();
        assert_eq!(shown.len(), 6, "all three outcomes are shown afterwards");
        cur = served(&f).unwrap();
    }

    // a booster: the next 3 right answers earn double, and then it says so
    q.chests(
        5,
        [Outcome::Booster, Outcome::Coins(20), Outcome::Coins(20)],
    );
    reply(p, &cur, right_mask(cur.id)).await;
    settle().await;
    verdict(&p.frames()).unwrap();
    p.send(ClientFrame::QuestPick {
        serial: cur.serial,
        chest: 0,
    })
    .await;
    settle().await;
    let f = p.frames();
    assert_eq!(chest_result(&f).unwrap()[3], json!(2), "booster");
    cur = served(&f).unwrap();
    let mut doubled = Vec::new();
    for n in 6..=8u32 {
        q.chests(n, [Outcome::Coins(20); 3]);
        reply(p, &cur, right_mask(cur.id)).await;
        settle().await;
        let f = p.frames();
        let v = verdict(&f).unwrap();
        doubled.push((v[3].as_i64().unwrap(), v[7].clone()));
        if n == 8 {
            assert_eq!(
                last_of(&f, 27).unwrap(),
                &json!([27, 2]),
                "the booster ran out"
            );
        } else {
            assert!(last_of(&f, 27).is_none());
        }
        p.send(ClientFrame::QuestPick {
            serial: cur.serial,
            chest: 0,
        })
        .await;
        settle().await;
        cur = served(&p.frames()).unwrap();
    }
    // 2 x (10 + speed 9..10 + bonus 25) each time the streak is past 5
    assert!(
        doubled.iter().all(|(e, _)| (2 * 44..=2 * 45).contains(e)),
        "{doubled:?}"
    );
    assert_eq!(
        doubled.iter().map(|d| d.1.clone()).collect::<Vec<_>>(),
        vec![json!(2), json!(1), json!(0)],
        "boosters left"
    );

    // the 9th answer is no longer doubled
    q.chests(9, [Outcome::Coins(20); 3]);
    reply(p, &cur, right_mask(cur.id)).await;
    settle().await;
    let e = verdict(&p.frames()).unwrap()[3].as_i64().unwrap();
    assert!((10 + 9 + 25..=10 + 10 + 25).contains(&e), "{e}");
    q.script_is_used_up();
}

#[tokio::test(start_paused = true)]
async fn a_slip_never_takes_a_player_below_zero() {
    let q = Quest::new();
    let (_h, mut ps, firsts) = running(&q, 2, 0).await;
    let p = &mut ps[0];
    let s = firsts[0].clone();
    q.chests(1, [Outcome::Slip; 3]);
    reply(p, &s, right_mask(s.id)).await;
    settle().await;
    let earned = verdict(&p.frames()).unwrap()[3].as_i64().unwrap();
    p.send(ClientFrame::QuestPick {
        serial: s.serial,
        chest: 2,
    })
    .await;
    settle().await;
    let c = chest_result(&p.frames()).unwrap();
    assert_eq!(c[3], json!(5), "slip");
    // 20 coins are lost, or all of them when the player has fewer
    assert_eq!(c[4].as_i64().unwrap(), earned.min(20));
    assert_eq!(c[5].as_i64().unwrap(), (earned - 20).max(0));
}

#[tokio::test(start_paused = true)]
async fn the_floor_holds_for_a_player_with_nothing() {
    let q = Quest::new();
    let (_h, mut ps, firsts) = running(&q, 2, 0).await;
    // p2 answers wrong so has nothing, then rights one for 19-20 coins and
    // slips with them: coins would go to 0 or stay positive, never negative
    let p = &mut ps[1];
    let s = firsts[1].clone();
    reply(p, &s, wrong_mask(s.id)).await;
    settle().await;
    p.frames();
    adv(Duration::from_millis(PAUSE_MS + 10)).await;
    let s = served(&p.frames()).unwrap();
    q.chests(1, [Outcome::Slip; 3]);
    reply(p, &s, right_mask(s.id)).await;
    settle().await;
    let earned = verdict(&p.frames()).unwrap()[3].as_i64().unwrap();
    assert!(earned < 20 + 1);
    p.send(ClientFrame::QuestPick {
        serial: s.serial,
        chest: 0,
    })
    .await;
    settle().await;
    let c = chest_result(&p.frames()).unwrap();
    assert_eq!(
        c[4].as_i64().unwrap(),
        earned.min(20),
        "only what they had is lost"
    );
    assert_eq!(
        c[5].as_i64().unwrap(),
        (earned - 20).max(0),
        "coins never below 0"
    );
}

/// Both players answer right at once; `a` ends up ahead of `b` by a booster
/// of coins, so ranks are known.
async fn two_players_with_a_leader() -> (Quest, Client, Vec<Client>, Vec<Served>) {
    let q = Quest::new();
    let (h, mut ps, mut cur) = running(&q, 3, 0).await;
    // p1 (index 0) gets 200 from a jackpot and leads; p2 and p3 have 0
    let s = cur[0].clone();
    q.chests(1, [Outcome::Jackpot; 3]);
    reply(&ps[0], &s, right_mask(s.id)).await;
    settle().await;
    ps[0].frames();
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    settle().await;
    cur[0] = served(&ps[0].frames()).unwrap();
    q.script_is_used_up();
    (q, h, ps, cur)
}

#[tokio::test(start_paused = true)]
async fn a_steal_takes_ten_percent_from_a_player_ranked_above() {
    let (q, _h, mut ps, cur) = two_players_with_a_leader().await;
    // p2 (rank 2 or 3) answers right and finds a steal
    let s = cur[1].clone();
    q.chests(1, [Outcome::Steal; 3]);
    // then the random pick among those ranked above the thief: they rank
    // second or third, so the leader is index 0 of the players above
    q.script.lock().unwrap().push_back(0);
    reply(&ps[1], &s, right_mask(s.id)).await;
    settle().await;
    ps[1].frames();
    ps[0].frames();
    ps[1]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    settle().await;
    let c = chest_result(&ps[1].frames()).unwrap();
    assert_eq!(c[3], json!(4), "steal");
    let taken = c[4].as_i64().unwrap();
    assert!((20..=22).contains(&taken), "10% of about 210: {taken}");
    assert_eq!(c[7], json!("name-p1"), "from the leader");
    assert_eq!(c[8], json!(0), "not blocked");
    // the victim is told, nobody else is
    let f = ps[0].frames();
    let ev = last_of(&f, 27).expect("the victim is told");
    assert_eq!(ev[1], json!(0));
    assert_eq!(ev[2], json!("name-p2"));
    assert_eq!(ev[3].as_i64().unwrap(), taken);
    assert!(
        ps[2]
            .frames()
            .iter()
            .all(|f| kind(f) != 27 && kind(f) != 26),
        "bystanders see nothing"
    );
    q.script_is_used_up();
}

#[tokio::test(start_paused = true)]
async fn a_shield_blocks_one_steal_and_both_sides_are_told() {
    let q = Quest::new();
    let (_h, mut ps, cur) = running(&q, 2, 0).await;
    // p1 earns a shield
    let s = cur[0].clone();
    q.chests(1, [Outcome::Shield; 3]);
    reply(&ps[0], &s, right_mask(s.id)).await;
    settle().await;
    ps[0].frames();
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    settle().await;
    let f = ps[0].frames();
    assert_eq!(chest_result(&f).unwrap()[3], json!(3));
    // p2 steals from p1 (the only one who can be above; p1 is ahead)
    let t = cur[1].clone();
    q.chests(1, [Outcome::Steal; 3]);
    // the three chest rolls first (pushed by chests()), then the victim: p1 is
    // the only one above p2 (p3 has the same 0 coins and joined later)
    q.script.lock().unwrap().push_back(0);
    //
    reply(&ps[1], &t, right_mask(t.id)).await;
    settle().await;
    ps[1].frames();
    ps[0].frames();
    ps[1]
        .send(ClientFrame::QuestPick {
            serial: t.serial,
            chest: 2,
        })
        .await;
    settle().await;
    let c = chest_result(&ps[1].frames()).unwrap();
    assert_eq!(
        (c[3].clone(), c[4].clone(), c[8].clone()),
        (json!(4), json!(0), json!(1)),
        "blocked, nothing taken"
    );
    assert_eq!(c[7], json!("name-p1"));
    let f = ps[0].frames();
    assert_eq!(
        last_of(&f, 27).unwrap(),
        &json!([27, 1, "name-p2"]),
        "the shield is told it worked"
    );
    q.script_is_used_up();
}

#[tokio::test(start_paused = true)]
async fn the_leader_steals_from_the_runner_up_and_a_lone_player_cannot_steal() {
    let q = Quest::new();
    let (_h, mut ps, cur) = running(&q, 2, 0).await;
    // give p2 some coins first so the runner-up has something to lose
    let t = cur[1].clone();
    q.chests(1, [Outcome::Jackpot; 3]);
    reply(&ps[1], &t, right_mask(t.id)).await;
    settle().await;
    ps[1].frames();
    ps[1]
        .send(ClientFrame::QuestPick {
            serial: t.serial,
            chest: 0,
        })
        .await;
    settle().await;
    ps[1].frames();
    // p1 answers right and gets a steal; p1 has 0 and is last, so it steals from p2
    let s = cur[0].clone();
    q.chests(1, [Outcome::Steal; 3]);
    q.script.lock().unwrap().push_back(0); // the one player above
    reply(&ps[0], &s, right_mask(s.id)).await;
    settle().await;
    ps[0].frames();
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    settle().await;
    let c = chest_result(&ps[0].frames()).unwrap();
    assert_eq!(c[7], json!("name-p2"));
    assert!(c[4].as_i64().unwrap() > 0);
    q.script_is_used_up();
}

#[tokio::test(start_paused = true)]
async fn friendly_mode_turns_steals_into_coins_and_a_lone_player_never_draws_one() {
    // friendly: the steal that comes up is replaced by coins (the roll for
    // the replacement is scripted to the first coin outcome)
    let q = Quest::new();
    let (_h, mut ps, cur) = running(&q, 2, 1).await;
    let s = cur[0].clone();
    {
        let mut sc = q.script.lock().unwrap();
        sc.push_back(roll_for(1, Outcome::Steal)); // chest 1: a steal, replaced...
        sc.push_back(0); // ...by the first coin outcome
        sc.push_back(roll_for(1, Outcome::Coins(40)));
        sc.push_back(roll_for(1, Outcome::Slip));
    }
    reply(&ps[0], &s, right_mask(s.id)).await;
    settle().await;
    ps[0].frames();
    q.script_is_used_up();
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    settle().await;
    let c = chest_result(&ps[0].frames()).unwrap();
    assert_eq!(c[3], json!(0), "coins instead of a steal");
    assert_eq!(c[4], json!(20));
    assert!(
        c[6].as_array().unwrap().chunks(2).all(|k| k[0] != json!(4)),
        "no steal anywhere"
    );
    assert_eq!(c[7], Value::Null);
    assert!(ps[1].frames().iter().all(|f| kind(f) != 27));

    // a lone player in a normal game: no steal can be drawn either
    let solo = Quest::new();
    let (_h2, mut ps2, cur2) = running(&solo, 1, 0).await;
    let s2 = cur2[0].clone();
    {
        let mut sc = solo.script.lock().unwrap();
        sc.push_back(roll_for(1, Outcome::Steal));
        sc.push_back(0);
        sc.push_back(0);
        sc.push_back(0);
    }
    reply(&ps2[0], &s2, right_mask(s2.id)).await;
    settle().await;
    ps2[0].frames();
    ps2[0]
        .send(ClientFrame::QuestPick {
            serial: s2.serial,
            chest: 0,
        })
        .await;
    settle().await;
    let c = chest_result(&ps2[0].frames()).unwrap();
    assert_eq!(c[3], json!(0), "nobody to steal from: coins");
}

#[tokio::test(start_paused = true)]
async fn requests_that_do_not_fit_are_refused() {
    let q = Quest::new();
    let (mut h, mut ps, cur) = running(&q, 2, 0).await;
    let s = cur[0].clone();

    // a stale serial, a pick without a chest, a pick and a next in the way
    ps[0]
        .send(ClientFrame::QuestAnswer {
            serial: s.serial + 1,
            mask: 1,
        })
        .await;
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    settle().await;
    assert_eq!(ps[0].frames(), vec![json!([8, 5, 6]), json!([8, 5, 7])]);
    // an invalid choice of options does not use up the question
    ps[0]
        .send(ClientFrame::QuestAnswer {
            serial: s.serial,
            mask: 0,
        })
        .await;
    ps[0]
        .send(ClientFrame::QuestAnswer {
            serial: s.serial,
            mask: 1 << 7,
        })
        .await;
    settle().await;
    assert_eq!(ps[0].frames(), vec![json!([8, 4, 6]), json!([8, 4, 6])]);
    // the question can be asked for again, with the time left
    adv(Duration::from_secs(5)).await;
    ps[0].send(ClientFrame::QuestNext).await;
    settle().await;
    let again = served(&ps[0].frames()).unwrap();
    assert_eq!(again.serial, s.serial);
    assert!(again.remaining_ms <= limit_of(s.id) * 1000 - 5000);

    // right answer: a second answer and an early next question are refused
    q.chests(1, [Outcome::Coins(20); 3]);
    reply(&ps[0], &s, right_mask(s.id)).await;
    settle().await;
    ps[0].frames();
    reply(&ps[0], &s, right_mask(s.id)).await;
    ps[0].send(ClientFrame::QuestNext).await;
    settle().await;
    assert_eq!(
        ps[0].frames(),
        vec![json!([8, 5, 6]), json!([8, 5, 8])],
        "answer again; next before the pick"
    );
    // a pick of a chest that does not exist
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 3,
        })
        .await;
    settle().await;
    assert_eq!(ps[0].frames(), vec![json!([8, 4, 7])]);
    // the pick, then the same pick again
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    settle().await;
    let f = ps[0].frames();
    assert!(chest_result(&f).is_some());
    assert_eq!(
        f.last().unwrap(),
        &json!([8, 5, 7]),
        "a chest is picked once"
    );

    // roles: the host does not play, players do not run the game, the
    // race's frames do not exist here
    h.send(ClientFrame::QuestAnswer { serial: 1, mask: 1 })
        .await;
    ps[1].send(ClientFrame::Next).await;
    ps[1].send(ClientFrame::End).await;
    ps[1].send(ClientFrame::Answer { q: 1, mask: 1 }).await;
    ps[1]
        .send(ClientFrame::StartQuest {
            quiz_id: 2,
            seconds: 300,
            flags: 0,
        })
        .await;
    settle().await;
    assert_eq!(h.frames().iter().filter(|f| kind(f) == 8).count(), 1);
    let f: Vec<Value> = ps[1]
        .frames()
        .into_iter()
        .filter(|f| kind(f) != 28)
        .collect();
    assert_eq!(f[0], json!([8, 2, 2]));
    assert_eq!(f[1], json!([8, 2, 3]));
    assert_eq!(f[2], json!([8, 5, 4]));
    assert_eq!(f[3], json!([8, 2, 1]), "a player cannot start a game");
}

#[tokio::test(start_paused = true)]
async fn nothing_about_another_players_question_or_chest_reaches_anybody_else() {
    let q = Quest::new();
    let (mut h, mut ps, cur) = running(&q, 3, 0).await;
    let s = cur[0].clone();
    q.chests(1, [Outcome::Coins(20), Outcome::Jackpot, Outcome::Booster]);
    reply(&ps[0], &s, right_mask(s.id)).await;
    settle().await;
    ps[0]
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 1,
        })
        .await;
    settle().await;
    adv(Duration::from_millis(100)).await;
    // others got nothing at all for another player's answers; the host only
    // the board
    for p in ps.iter_mut().skip(1) {
        let f = p.frames();
        assert!(
            f.iter().all(|f| ![24, 25, 26, 27].contains(&kind(f))),
            "a bystander was sent {f:?}"
        );
    }
    let f = h.frames();
    assert!(
        f.iter().all(|f| ![24, 25, 26, 27].contains(&kind(f))),
        "{f:?}"
    );
    assert!(last_of(&f, 29).is_some(), "the board moved");
    // the answer key of an open question is nowhere in what players got
    // before they answered: a question frame is the shared prefix + serial
    let q2 = cur[1].clone();
    assert_eq!(q2.serial, 1);
}

#[tokio::test(start_paused = true)]
async fn the_host_board_and_the_players_strip_are_throttled_and_only_when_changed() {
    let q = Quest::new();
    let (mut h, mut ps, cur) = running(&q, 4, 0).await;
    adv(Duration::from_millis(600)).await;
    h.frames();
    for p in &mut ps {
        p.frames();
    }
    // four answers within 100 ms: the board goes out at once and then once
    // more after 500 ms
    adv(Duration::from_millis(MIN_THINK_MS)).await; // nobody answers sooner
    h.frames();
    for (i, s) in cur.iter().enumerate() {
        q.chests(1, [Outcome::Coins(20); 3]);
        ps[i]
            .send(ClientFrame::QuestAnswer {
                serial: s.serial,
                mask: right_mask(s.id),
            })
            .await;
        settle().await;
        adv(Duration::from_millis(20)).await;
    }
    let f = h.frames();
    assert_eq!(
        f.iter().filter(|f| kind(f) == 29).count(),
        1,
        "the first change at once"
    );
    adv(Duration::from_millis(600)).await;
    let f = h.frames();
    let boards: Vec<_> = f.iter().filter(|f| kind(f) == 29).collect();
    assert_eq!(boards.len(), 1, "then one with the rest");
    let board = boards[0];
    assert_eq!(board[2], json!(4), "players");
    assert_eq!(board[3], json!(4), "answers");
    assert_eq!(board[4], json!(4), "correct");
    assert!(board[1].as_array().unwrap().len() <= 10);
    adv(Duration::from_secs(1)).await;
    assert!(
        h.frames().iter().all(|f| kind(f) != 29),
        "nothing changed, nothing sent"
    );

    // the standings strip: at most every 3 s, and the shared part is the same
    // bytes for everyone
    adv(STANDINGS_EVERY).await;
    let strips: Vec<Value> = ps
        .iter_mut()
        .filter_map(|p| last_of(&p.frames(), 28).cloned())
        .collect();
    assert_eq!(strips.len(), 4, "everybody's coins changed");
    assert!(
        strips.iter().all(|s| s[1] == strips[0][1]),
        "the same top five"
    );
    assert_eq!(strips[0][1].as_array().unwrap().len(), 4);
    let ranks: Vec<_> = strips.iter().map(|s| s[2].as_i64().unwrap()).collect();
    assert_eq!(ranks.iter().sum::<i64>(), 1 + 2 + 3 + 4);
    // quiet: nothing changed, nothing sent
    adv(STANDINGS_EVERY).await;
    for p in &mut ps {
        assert!(p.frames().iter().all(|f| kind(f) != 28));
    }
}

#[tokio::test(start_paused = true)]
async fn a_player_who_reconnects_or_joins_late_is_brought_up_to_date() {
    let q = Quest::new();
    let (_h, mut ps, cur) = running(&q, 2, 0).await;
    let s = cur[0].clone();
    adv(Duration::from_secs(4)).await;
    // p1 drops while a question is open and comes back
    ps[0].room.detach(ps[0].sock).await;
    settle().await;
    let mut back = q.player(1).await;
    let f = back.frames();
    assert_eq!(kind(&f[0]), 22, "the game is running");
    let me = last_of(&f, 23).unwrap();
    assert_eq!(me[8], json!(1), "a question is open");
    let again = served(&f).unwrap();
    assert_eq!(again.serial, s.serial, "the same question");
    assert!(
        again.remaining_ms < limit_of(s.id) * 1000 - 3000,
        "with the time that is left"
    );

    // right answer, then drop while the chest is waiting
    q.chests(1, [Outcome::Coins(20); 3]);
    reply(&back, &s, right_mask(s.id)).await;
    settle().await;
    back.room.detach(back.sock).await;
    settle().await;
    let mut third = q.player(1).await;
    let f = third.frames();
    let me = last_of(&f, 23).unwrap();
    assert_eq!(me[8], json!(2), "a chest is waiting");
    assert!(me[1].as_i64().unwrap() > 0, "with the coins already earned");
    third
        .send(ClientFrame::QuestPick {
            serial: s.serial,
            chest: 0,
        })
        .await;
    settle().await;
    assert!(chest_result(&third.frames()).is_some());

    // somebody who comes in the middle starts at zero with a question
    let mut late = q.player(9).await;
    let f = late.frames();
    let me = last_of(&f, 23).unwrap();
    assert_eq!(me[1], json!(0));
    assert_eq!(served(&f).unwrap().serial, 1);
    ps[1].frames();
}

#[tokio::test(start_paused = true)]
async fn at_the_end_the_podium_is_shown_and_the_results_are_stored_in_one_write() {
    let q = Quest::new();
    let (mut h, mut ps, cur) = running(&q, 3, 0).await;
    // p1 answers right three times, p2 once, p3 is wrong
    let mut now_cur = cur.clone();
    for round in 1..=3u32 {
        let s = now_cur[0].clone();
        q.chests(round, [Outcome::Coins(20); 3]);
        reply(&ps[0], &s, right_mask(s.id)).await;
        settle().await;
        ps[0].frames();
        ps[0]
            .send(ClientFrame::QuestPick {
                serial: s.serial,
                chest: 0,
            })
            .await;
        settle().await;
        now_cur[0] = served(&ps[0].frames()).unwrap();
        h.frames();
    }
    q.chests(1, [Outcome::Coins(20); 3]);
    reply(&ps[1], &cur[1], right_mask(cur[1].id)).await;
    reply(&ps[2], &cur[2], wrong_mask(cur[2].id)).await;
    settle().await;
    ps[1].frames();
    h.frames();

    assert!(
        q.w.store.batches.lock().unwrap().is_empty(),
        "nothing is written while it runs"
    );
    adv(Duration::from_secs(300)).await;
    // time is up: the podium, with coins as the score
    let f = ps[0].frames();
    let podium = last_of(&f, 6).expect("the podium");
    assert_eq!(podium[1][0][0], json!("name-p1"));
    assert!(podium[1][0][1].as_i64().unwrap() > 60);
    assert_eq!(podium.as_array().unwrap()[3], json!(1), "rank");
    let fh = h.frames();
    assert_eq!(
        last_of(&fh, 6).unwrap().as_array().unwrap().len(),
        3,
        "the full order for the host"
    );

    // questions no longer count
    reply(&ps[0], &now_cur[0], right_mask(now_cur[0].id)).await;
    settle().await;
    assert_eq!(ps[0].frames(), vec![json!([8, 5, 6])]);
    assert_eq!(
        q.w.store.batches.lock().unwrap().len(),
        1,
        "the results were stored when the podium opened"
    );

    h.send(ClientFrame::Next).await;
    settle().await;
    assert_eq!(last_of(&ps[1].frames(), 7).unwrap(), &json!([7, 0]));
    let batches = q.w.store.batches.lock().unwrap();
    assert_eq!(
        batches.len(),
        1,
        "the results are written in ONE transaction"
    );
    let b = &batches[0];
    assert!(b.ended_at.is_some() && b.answers.is_empty());
    assert_eq!(b.players.len(), 3);
    let p1 = b
        .players
        .iter()
        .find(|p| p.user_id.as_deref() == Some("p1"))
        .unwrap();
    assert_eq!((p1.correct_count, p1.answered, p1.best_streak), (3, 3, 3));
    assert!(p1.score > 60, "coins are the score");
    let p3 = b
        .players
        .iter()
        .find(|p| p.user_id.as_deref() == Some("p3"))
        .unwrap();
    assert_eq!(
        (p3.score, p3.correct_count, p3.answered, p3.best_streak),
        (0, 0, 1, 0)
    );
    assert_eq!(
        q.w.store.games.lock().unwrap()[0].mode,
        1,
        "stored as a quest"
    );
}

#[tokio::test(start_paused = true)]
async fn the_host_can_end_a_quest_early_and_it_ends_by_itself_at_the_room_close() {
    let q = Quest::new();
    let (mut h, mut ps, _) = running(&q, 2, 0).await;
    h.send(ClientFrame::End).await;
    settle().await;
    // ending a game in play shows the podium; a second end ends it
    assert!(last_of(&ps[0].frames(), 6).is_some());
    h.send(ClientFrame::End).await;
    settle().await;
    assert_eq!(last_of(&ps[0].frames(), 7).unwrap(), &json!([7, 0]));
    assert_eq!(q.w.store.batches.lock().unwrap().len(), 1, "stored once");
    h.frames();

    // a new game can follow in the same room, and the room's close ends it
    start(&h, 900, 0).await;
    settle().await;
    ps[0].frames();
    h.send(ClientFrame::Next).await;
    settle().await;
    adv(Duration::from_secs(900)).await; // the time is up: the podium
    assert!(last_of(&ps[0].frames(), 6).is_some());
    adv(Duration::from_secs(60 * 60 + 5)).await; // idle for an hour
    assert_eq!(last_of(&ps[0].frames(), 7).unwrap(), &json!([7, 2]));
    assert_eq!(q.w.store.batches.lock().unwrap().len(), 2, "one per game");
}

#[tokio::test(start_paused = true)]
async fn one_game_per_room_and_the_race_still_works_afterwards() {
    let q = Quest::new();
    let (mut h, mut ps, _) = running(&q, 1, 0).await;
    start(&h, 300, 0).await;
    h.send(ClientFrame::Start(2)).await;
    settle().await;
    let f = h.frames();
    assert_eq!(f.iter().filter(|f| **f == json!([8, 5, 1])).count(), 2);
    h.send(ClientFrame::End).await; // the podium
    h.send(ClientFrame::End).await; // the end
    settle().await;
    ps[0].frames();
    // a race on the same quiz in the same room, afterwards
    h.send(ClientFrame::Start(2)).await;
    settle().await;
    assert!(last_of(&ps[0].frames(), 1).is_some(), "the race's lobby");
}

#[tokio::test(start_paused = true)]
async fn an_answer_sooner_than_800_ms_after_the_question_is_refused_and_the_question_stays_open() {
    let q = Quest::new();
    let (_h, mut ps, cur) = running(&q, 1, 0).await;
    let p = &mut ps[0];
    let s = cur[0].clone();
    let send = |mask: u32| ClientFrame::QuestAnswer {
        serial: s.serial,
        mask,
    };
    // 1 ms after it was served: a script, not a student
    p.send(send(right_mask(s.id))).await;
    settle().await;
    assert_eq!(p.frames(), vec![json!([8, 5, 6])]);
    adv(Duration::from_millis(MIN_THINK_MS - 100)).await;
    p.send(send(right_mask(s.id))).await;
    settle().await;
    assert_eq!(
        p.frames(),
        vec![json!([8, 5, 6])],
        "799 ms is still too soon"
    );

    // refused, not spent: a little later the same question takes the answer
    q.chests(1, [Outcome::Coins(20); 3]);
    adv(Duration::from_millis(200)).await;
    p.send(send(right_mask(s.id))).await;
    settle().await;
    let v = verdict(&p.frames()).expect("accepted once 800 ms have passed");
    assert_eq!(v[2], json!(1));
    // refusals did not count as answers
    let board = q.w.store.batches.lock().unwrap().len();
    assert_eq!(board, 0);
}

#[tokio::test(start_paused = true)]
async fn an_answer_received_after_the_end_of_the_game_is_refused_even_before_the_clock_fires() {
    use super::{Fx, Quest as Game};
    let now = Instant::now();
    let mut game = Game::new(
        1,
        "t".into(),
        "teacher".into(),
        quest_quiz().questions,
        300,
        false,
        Box::new(SeededDraw(3)),
        now + Duration::from_secs(7200),
        now,
    );
    let pid = game.join("p1", "P1", 0).unwrap();
    let mut fx = Fx::default();
    game.begin(now, 0, &mut fx);
    let ends_at = now + Duration::from_secs(300);
    // a question served five seconds before the end still has its own time
    // left when the game's time is up
    game.serve(pid, ends_at - Duration::from_secs(5), &mut fx);
    let serial = game.players[pid].serial;
    let id = match game.players[pid].state {
        super::PState::Asking { q, deadline, .. } => {
            assert!(
                deadline > ends_at + Duration::from_secs(4),
                "the question outlives the game"
            );
            game.questions[q].question.id
        }
        _ => panic!("a question is open"),
    };
    let right = right_mask(id);
    // 1 ms after the end: refused although the phase is still Running
    let refused = game.answer(
        pid,
        serial,
        right,
        ends_at + Duration::from_millis(1),
        &mut fx,
    );
    assert!(
        matches!(refused, Err(crate::error::AppError::Conflict(_))),
        "{refused:?}"
    );
    assert_eq!(game.answers, 0, "it was not counted");
    // a second before the end it is taken
    assert!(
        game.answer(
            pid,
            serial,
            right,
            ends_at - Duration::from_secs(1),
            &mut fx
        )
        .is_ok()
    );
}

/// Batches that carry the end of a game.
fn ended_writes(q: &Quest) -> usize {
    q.w.store
        .batches
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.ended_at.is_some())
        .count()
}

#[tokio::test(start_paused = true)]
async fn ending_a_quest_in_play_shows_the_podium_stores_the_results_once_and_ends_on_the_next_end()
{
    let q = Quest::new();
    let (mut h, mut ps, cur) = running(&q, 3, 0).await;
    // p1 answers right (the chest is left unopened), p2 wrong
    q.chests(1, [Outcome::Jackpot; 3]);
    reply(&ps[0], &cur[0], right_mask(cur[0].id)).await;
    reply(&ps[1], &cur[1], wrong_mask(cur[1].id)).await;
    settle().await;
    for c in ps.iter_mut() {
        c.frames();
    }
    h.frames();
    assert_eq!(ended_writes(&q), 0);

    h.send(ClientFrame::End).await;
    settle().await;
    // the podium: coins as the score, the full order for the host, players
    // their top three and their own place; the game is not over yet
    let f = ps[0].frames();
    let podium = last_of(&f, 6).expect("a podium");
    assert_eq!(podium[1][0][0], json!("name-p1"));
    assert!(
        podium[1][0][1].as_i64().unwrap() >= 10,
        "the answer's coins"
    );
    assert_eq!(
        podium.as_array().unwrap().len(),
        4,
        "top three, coins, rank"
    );
    assert_eq!(
        last_of(&h.frames(), 6).unwrap().as_array().unwrap().len(),
        3
    );
    assert!(last_of(&f, 7).is_none(), "not ended yet");
    assert_eq!(ended_writes(&q), 1, "stored when the podium opened");
    // answers no longer count
    reply(&ps[2], &cur[2], right_mask(cur[2].id)).await;
    settle().await;
    assert_eq!(last_of(&ps[2].frames(), 8).unwrap(), &json!([8, 5, 6]));

    // pressing end again ends it, and nothing is stored a second time
    h.send(ClientFrame::End).await;
    settle().await;
    assert_eq!(last_of(&ps[0].frames(), 7).unwrap(), &json!([7, 0]));
    assert_eq!(ended_writes(&q), 1, "stored exactly once");
    assert!(q.w.deps.rooms.running_quiz_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn ending_a_quest_in_the_lobby_ends_it_at_once() {
    let q = Quest::new();
    let mut p = q.player(1).await;
    let mut h = q.host().await;
    start(&h, 300, 0).await;
    settle().await;
    p.frames();
    h.send(ClientFrame::End).await;
    settle().await;
    let f = p.frames();
    assert_eq!(last_of(&f, 7).unwrap(), &json!([7, 0]));
    assert!(last_of(&f, 6).is_none(), "nothing was played: no podium");
    assert_eq!(ended_writes(&q), 1);
    h.frames();
}

#[tokio::test(start_paused = true)]
async fn the_room_closing_or_idleness_during_the_podium_ends_a_quest_without_a_second_write() {
    let q = Quest::new();
    let (mut h, mut ps, _) = running(&q, 2, 0).await;
    h.send(ClientFrame::End).await;
    settle().await;
    ps[0].frames();
    assert_eq!(ended_writes(&q), 1);
    for _ in 0..70 {
        adv(Duration::from_secs(60)).await;
        h.frames();
        ps[0].frames();
        ps[1].frames();
    }
    // an hour without a command: the end
    assert_eq!(ended_writes(&q), 1, "still one write");
    assert!(q.w.deps.rooms.running_quiz_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_quest_host_away_in_the_lobby_ends_it_with_reason_4_and_no_podium() {
    let q = Quest::new();
    let mut p = q.player(1).await;
    let h = q.host().await;
    start(&h, 300, 0).await;
    settle().await;
    p.trace();
    leave(&h).await;
    adv(HOST_AWAY_LIMIT - secs(1)).await;
    assert!(p.trace().is_empty());
    adv(secs(2)).await;
    let t = p.trace();
    assert_eq!(ended_reason(&t).as_deref(), Some("4"), "{t:?}");
    assert!(t.iter().all(|f| !f.starts_with("[6,")), "no podium: {t:?}");
    assert_eq!(ended_writes(&q), 1);
    assert!(q.w.deps.rooms.running_quiz_ids().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_quest_host_away_while_it_runs_shows_the_podium_stores_once_and_a_host_back_in_time_stays()
 {
    let q = Quest::new();
    let (h, mut ps, _) = running(&q, 2, 0).await;
    leave(&h).await;
    adv(HOST_AWAY_LIMIT - secs(5)).await;
    // back in time: nothing happens, however long the game goes on
    let again = q.host().await;
    adv(secs(60)).await;
    assert_eq!(ended_writes(&q), 0);
    assert!(ended_reason(&ps[0].trace()).is_none());
    leave(&again).await;
    adv(HOST_AWAY_LIMIT + secs(1)).await;
    let t = ps[0].trace();
    let podium = t.iter().position(|f| f.starts_with("[6,")).expect("podium");
    let ended = t.iter().position(|f| f == "[7,4]").expect("reason 4");
    assert!(podium < ended, "{t:?}");
    assert_eq!(ended_writes(&q), 1, "stored once");
    assert!(q.w.deps.rooms.running_quiz_ids().is_empty());
    ps[1].trace();
}

#[tokio::test(start_paused = true)]
async fn a_quest_host_away_on_the_podium_ends_it_without_a_second_write() {
    let q = Quest::new();
    let (h, mut ps, _) = running(&q, 2, 0).await;
    h.send(ClientFrame::End).await;
    settle().await;
    ps[0].trace();
    assert_eq!(ended_writes(&q), 1);
    leave(&h).await;
    adv(HOST_AWAY_LIMIT + secs(1)).await;
    assert_eq!(ended_reason(&ps[0].trace()).as_deref(), Some("4"));
    assert_eq!(ended_writes(&q), 1);
}

#[tokio::test(start_paused = true)]
async fn a_quest_with_no_socket_at_all_ends_after_five_minutes_and_frees_the_room() {
    let q = Quest::new();
    let (h, ps, _) = running(&q, 2, 0).await;
    leave(&h).await;
    for p in &ps {
        leave(p).await;
    }
    adv(EMPTY_ROOM_LIMIT - secs(2)).await;
    assert_eq!(q.w.deps.rooms.counts().running_games, 1);
    assert_eq!(ended_writes(&q), 0);
    adv(secs(4)).await;
    nothing_left(&q.w);
    assert_eq!(ended_writes(&q), 1, "stored once");
}

#[tokio::test(start_paused = true)]
async fn at_the_room_close_a_quest_ends_and_is_stored_before_its_sockets_close() {
    let q = Quest::new();
    let rc = BASE + 10 * 60_000;
    let mut p = connect_rc(&q.w, "p1", Role::Player, rc).await;
    let mut h = connect_rc(&q.w, "teacher", Role::Host, rc).await;
    start(&h, 900, 0).await;
    settle().await;
    p.trace();
    h.trace();
    // the host keeps the game alive; the room closes under it
    let mut t = Vec::new();
    for _ in 0..10 {
        adv(secs(60)).await;
        h.send(ClientFrame::Next).await;
        settle().await;
        h.trace();
        t.extend(p.trace());
    }
    assert!(t.iter().any(|f| f == "[7,1]"), "ended at rc: {t:?}");
    assert_eq!(ended_writes(&q), 1);
    adv(ROOM_CLOSE_GRACE + secs(1)).await;
    assert_eq!(p.trace(), vec![CLOSED]);
    nothing_left(&q.w);
}

#[tokio::test(start_paused = true)]
async fn a_quest_whose_last_write_failed_is_written_once_the_database_is_back() {
    let q = Quest::new();
    let (mut h, mut ps, _) = running(&q, 2, 0).await;
    *q.w.store.fail_writes.lock().unwrap() = true;
    h.send(ClientFrame::End).await; // the podium's write fails
    settle().await;
    h.send(ClientFrame::End).await; // so does the end's: the room lets the game go
    settle().await;
    assert_eq!(ended_writes(&q), 0);
    *q.w.store.fail_writes.lock().unwrap() = false;
    adv(Duration::from_secs(2)).await;
    assert_eq!(ended_writes(&q), 1, "written later, once");
    h.frames();
    ps[0].frames();
}
