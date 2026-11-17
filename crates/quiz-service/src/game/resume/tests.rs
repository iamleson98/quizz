//! Games that survive a restart: paused time, a fake store. A restart is a new set of rooms over
//! what the old process stored (see `restarted`).

use super::{Saved, VERSION, WRITE_TRIES, Writer};
use crate::auth::Role;
use crate::game::frames::ClientFrame;
use crate::game::tests::{
    BASE, adv, answer, connect, host, last_of, player, restarted, secs, settle, wall_since, world,
};
use crate::repo::running::RunningRow;
use serde_json::json;
use tokio::time::Instant;

fn row(updated_at: i64) -> RunningRow {
    RunningRow {
        room: "s1".into(),
        game_id: 1,
        kind: 0,
        version: i64::from(VERSION),
        snapshot: Vec::new(),
        updated_at,
    }
}

#[tokio::test(start_paused = true)]
async fn a_race_resumes_after_a_graceful_stop_with_its_answers_and_finishes() {
    let start = Instant::now();
    let w = world();
    let mut p1 = player(&w, 1).await;
    let _p2 = player(&w, 2).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    let deadline = last_of(&p1.frames(), 2).unwrap()[4].clone();
    adv(secs(5)).await;
    answer(&p1, 1, 0b0010).await;
    settle().await;
    p1.frames();

    w.deps.rooms.shutdown(secs(5)).await;
    let t = p1.trace();
    assert!(t.contains(&"close:1001:restart".to_string()), "{t:?}");
    assert!(
        t.iter().all(|f| !f.starts_with("[7,")),
        "the game did not end: {t:?}"
    );
    assert_eq!(w.store.running.lock().unwrap().len(), 1);
    assert!(w.store.batches.lock().unwrap().is_empty());

    adv(secs(2)).await;
    let w2 = restarted(&w, wall_since(start)).await;
    assert_eq!(w2.deps.rooms.counts().running_games, 1);
    let mut p1 = connect(&w2, "s1", "p1", Role::Player).await;
    let mut p2 = connect(&w2, "s1", "p2", Role::Player).await;
    let mut h = connect(&w2, "s1", "teacher", Role::Host).await;
    let f = p1.frames();
    let q = last_of(&f, 2).expect("the question is open");
    assert_eq!(q[2], json!(1));
    let pushed = q[4].as_i64().unwrap() - deadline.as_i64().unwrap();
    assert!(
        (22_000..22_010).contains(&pushed),
        "a planned stop: the 2 s it was down and the 20 s to reconnect are not taken ({pushed})"
    );
    assert_eq!(
        q.as_array().unwrap().last().unwrap(),
        &json!(0b0010),
        "p1's answer was kept"
    );
    let f = p2.frames();
    let q = last_of(&f, 2).expect("the question is open");
    assert_eq!(q.as_array().unwrap().last().unwrap(), &json!(0));
    assert!(
        last_of(&h.frames(), 4).is_some(),
        "the host gets the live counts"
    );

    answer(&p2, 1, 0b0001).await;
    settle().await;
    adv(secs(3)).await;
    let f = p1.frames();
    let reveal = last_of(&f, 5).expect("the reveal");
    assert_eq!(
        &reveal.as_array().unwrap()[6..],
        &[
            json!(875),
            json!(1),
            json!(875),
            json!(1),
            json!(2),
            json!(1),
            json!(0)
        ],
        "the score of the answer given before the stop"
    );

    h.send(ClientFrame::End).await;
    h.send(ClientFrame::End).await;
    settle().await;
    assert_eq!(last_of(&p1.frames(), 7).unwrap(), &json!([7, 0]));
    assert!(w2.store.running.lock().unwrap().is_empty(), "the row went");
    assert_eq!(w2.deps.rooms.counts().running_games, 0);
    let batches = w2.store.batches.lock().unwrap();
    assert!(batches.last().unwrap().ended_at.is_some());
    assert_eq!(
        batches
            .iter()
            .flat_map(|b| &b.players)
            .map(|p| p.score)
            .max(),
        Some(875),
        "the results hold the score"
    );
}

#[tokio::test(start_paused = true)]
async fn a_race_resumes_after_a_crash_and_closes_the_question_whose_time_passed_while_down() {
    let start = Instant::now();
    let w = world();
    let p1 = player(&w, 1).await;
    let _p2 = player(&w, 2).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    adv(secs(5)).await;
    answer(&p1, 1, 0b0010).await;
    adv(secs(4)).await;
    assert_eq!(w.store.running.lock().unwrap().len(), 1);

    // no shutdown: the process died; the question's 20 s ran out while it was down
    let w2 = restarted(&w, wall_since(start) + 30_000).await;
    adv(secs(3)).await;
    let mut p1 = connect(&w2, "s1", "p1", Role::Player).await;
    let f = p1.frames();
    let reveal = last_of(&f, 5).expect("the question was closed and revealed");
    assert_eq!(reveal[6], json!(875), "p1's answer scored");
    let batches = w2.store.batches.lock().unwrap();
    assert_eq!(batches.len(), 1, "the question's answers were written");
    assert_eq!(batches[0].answers.len(), 1);
    assert_eq!(batches[0].players.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_lobby_resumes_and_players_join_it_again() {
    let start = Instant::now();
    let w = world();
    let _p1 = player(&w, 1).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    w.deps.rooms.shutdown(secs(5)).await;

    let w2 = restarted(&w, wall_since(start) + 1_000).await;
    let mut h = connect(&w2, "s1", "teacher", Role::Host).await;
    let f = h.frames();
    assert_eq!(last_of(&f, 1).unwrap()[3], json!(3), "the lobby is back");
    let mut p1 = connect(&w2, "s1", "p1", Role::Player).await;
    assert_eq!(last_of(&p1.frames(), 1).unwrap()[4], json!(1));
    h.send(ClientFrame::Next).await;
    settle().await;
    assert!(last_of(&p1.frames(), 2).is_some(), "the game goes on");
}

#[tokio::test(start_paused = true)]
async fn the_row_goes_when_the_game_ends() {
    let w = world();
    let _p1 = player(&w, 1).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    assert_eq!(w.store.running.lock().unwrap().len(), 1);
    h.send(ClientFrame::End).await;
    settle().await;
    assert!(w.store.running.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_row_that_cannot_be_read_is_dropped_and_its_game_is_not_resumed() {
    let start = Instant::now();
    let w = world();
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    let good = w.store.running.lock().unwrap().get("s1").cloned().unwrap();

    type Spoil = fn(&mut RunningRow, i64);
    let spoilt: [(&str, Spoil); 3] = [
        ("another version", |r, _| r.version = i64::from(VERSION) + 1),
        ("damaged", |r, _| r.snapshot.truncate(10)),
        ("too old", |r, now| {
            r.updated_at = now - 2 * 24 * 60 * 60 * 1000
        }),
    ];
    for (why, spoil) in spoilt {
        let now = wall_since(start);
        let mut bad = good.clone();
        spoil(&mut bad, now);
        w.store.running.lock().unwrap().insert("s1".into(), bad);
        let w2 = restarted(&w, now).await;
        let counts = w2.deps.rooms.counts();
        assert_eq!((counts.rooms, counts.running_games), (0, 0), "{why}");
        assert!(
            w2.store.running.lock().unwrap().is_empty(),
            "{why}: dropped"
        );
    }
    // and the good one is taken
    w.store.running.lock().unwrap().insert("s1".into(), good);
    let w2 = restarted(&w, wall_since(start)).await;
    assert_eq!(w2.deps.rooms.counts().running_games, 1);
}

#[tokio::test(start_paused = true)]
async fn changes_within_a_phase_are_written_at_most_every_few_seconds() {
    let w = world();
    let p1 = player(&w, 1).await;
    let p2 = player(&w, 2).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    let writes = || *w.store.running_writes.lock().unwrap();
    let phases = writes();
    answer(&p1, 1, 0b0010).await;
    adv(secs(1)).await;
    assert_eq!(writes(), phases, "an answer is not written at once");
    answer(&p2, 1, 0b0001).await;
    adv(secs(1)).await;
    // both answered: the question closed, a phase change, written at once
    assert_eq!(writes(), phases + 1);
}

#[tokio::test(start_paused = true)]
async fn the_writer_keeps_only_the_latest_order_and_flush_waits_for_it() {
    let w = world();
    *w.store.save_delay.lock().unwrap() = secs(1);
    let mut writer = Writer::start(w.store.clone());
    for n in 0..100 {
        writer.save(row(n));
    }
    settle().await;
    // the first write is under way; more orders pile into one
    for n in 100..200 {
        writer.save(row(n));
    }
    writer.delete("elsewhere".into(), 1);
    writer.save(row(999));
    writer.flush().await;
    assert_eq!(
        *w.store.running_writes.lock().unwrap(),
        2,
        "two writes, not 202"
    );
    assert_eq!(w.store.running.lock().unwrap()["s1"].updated_at, 999);
}

#[tokio::test(start_paused = true)]
async fn the_writer_of_a_dropped_room_retries_with_backoff_then_gives_up() {
    let w = world();
    *w.store.fail_running.lock().unwrap() = true;
    let mut writer = Writer::start(w.store.clone());
    writer.save(row(1));
    drop(writer);
    for _ in 0..5 {
        adv(secs(1)).await;
    }
    let tries = *w.store.running_tries.lock().unwrap();
    assert!(
        (2..WRITE_TRIES).contains(&tries),
        "backing off, not hammering: {tries} tries after 5 s"
    );
    for _ in 0..120 {
        adv(secs(1)).await;
    }
    assert_eq!(*w.store.running_tries.lock().unwrap(), WRITE_TRIES);
}

#[tokio::test(start_paused = true)]
async fn a_failing_database_is_tried_again_and_a_newer_state_replaces_the_one_failing() {
    let w = world();
    *w.store.fail_running.lock().unwrap() = true;
    let mut writer = Writer::start(w.store.clone());
    writer.save(row(1));
    adv(secs(2)).await;
    *w.store.fail_running.lock().unwrap() = false;
    writer.save(row(2));
    writer.flush().await;
    assert_eq!(w.store.running.lock().unwrap()["s1"].updated_at, 2);
}

#[tokio::test(start_paused = true)]
async fn a_delete_takes_only_the_row_of_its_own_game() {
    let w = world();
    let mut writer = Writer::start(w.store.clone());
    writer.save(row(1));
    writer.flush().await;
    writer.delete("s1".into(), 2);
    writer.flush().await;
    assert_eq!(
        w.store.running.lock().unwrap().len(),
        1,
        "the row is game 1's"
    );
    writer.delete("s1".into(), 1);
    writer.flush().await;
    assert!(w.store.running.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_next_game_of_a_room_keeps_its_row_when_the_one_before_ends() {
    let w = world();
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    let first = w.store.running.lock().unwrap()["s1"].game_id;
    h.send(ClientFrame::End).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    let rows = w.store.running.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_ne!(rows["s1"].game_id, first, "the second game's row");
}

#[tokio::test(start_paused = true)]
async fn a_game_that_ended_before_its_row_was_deleted_is_not_put_back() {
    let start = Instant::now();
    let w = world();
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    settle().await;
    w.deps.rooms.shutdown(secs(5)).await;
    // the end of the game reached the results, and the process died before the row was deleted
    let game_id = w.store.running.lock().unwrap()["s1"].game_id;
    w.store
        .batches
        .lock()
        .unwrap()
        .push(crate::repo::games::Batch {
            game_id,
            ended_at: Some(BASE),
            ..Default::default()
        });
    let w2 = restarted(&w, wall_since(start)).await;
    assert_eq!(w2.deps.rooms.counts().running_games, 0);
    assert!(
        w2.store.running.lock().unwrap().is_empty(),
        "the row was dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn nobody_attaches_while_the_service_stops() {
    let w = world();
    let _p = player(&w, 1).await;
    w.deps.rooms.shutdown(secs(5)).await;
    assert!(w.deps.rooms.closing());
    let late = crate::game::tests::try_connect(&w, "s1", "p2", Role::Player).await;
    assert!(late.is_err(), "no new room in a process that is going");
    let other = crate::game::tests::try_connect(&w, "s9", "p3", Role::Player).await;
    assert!(other.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_planned_stop_does_not_cost_a_race_player_answering_time_but_a_crash_does() {
    let start = Instant::now();
    let w = world();
    let mut p1 = player(&w, 1).await;
    let _p2 = player(&w, 2).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    let deadline = last_of(&p1.frames(), 2).unwrap()[4].as_i64().unwrap();
    adv(secs(5)).await;
    // the crash has the state as of the phase change, since no answer came
    let crashed = restarted(&w, wall_since(start) + 60_000).await;
    adv(secs(3)).await;
    let mut late = connect(&crashed, "s1", "p1", Role::Player).await;
    assert!(
        last_of(&late.frames(), 5).is_some(),
        "after a crash the question ran out while the service was down"
    );

    w.deps.rooms.shutdown(secs(5)).await;
    let paused = restarted(&w, wall_since(start) + 60_000).await;
    let mut back = connect(&paused, "s1", "p1", Role::Player).await;
    let f = back.frames();
    let q = last_of(&f, 2).expect("the question is still open");
    let pushed = q[4].as_i64().unwrap() - deadline;
    assert!(
        (80_000..80_100).contains(&pushed),
        "the question has the time it had left, and the allowance ({pushed})"
    );
    answer(&back, 1, 0b0010).await;
    settle().await;
    assert_eq!(
        last_of(&back.frames(), 9).unwrap(),
        &json!([9]),
        "and can be answered"
    );
}

#[tokio::test(start_paused = true)]
async fn after_a_planned_stop_players_have_the_allowance_to_come_back_and_then_stop_counting() {
    let start = Instant::now();
    let w = world();
    let p1 = player(&w, 1).await;
    let _p2 = player(&w, 2).await;
    let _p3 = player(&w, 3).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    adv(secs(2)).await;
    answer(&p1, 1, 0b0010).await;
    settle().await;
    w.deps.rooms.shutdown(secs(5)).await;

    let w2 = restarted(&w, wall_since(start) + 3_000).await;
    let mut p1 = connect(&w2, "s1", "p1", Role::Player).await;
    // p2 comes back 15 s after the restore, a web client's retry later than the plain grace
    adv(secs(15)).await;
    let mut p2 = connect(&w2, "s1", "p2", Role::Player).await;
    assert!(
        last_of(&p2.frames(), 2).is_some(),
        "the question is still open for them"
    );
    answer(&p2, 1, 0b0001).await;
    settle().await;
    assert_eq!(
        last_of(&p2.frames(), 9).unwrap(),
        &json!([9]),
        "and takes their answer"
    );
    assert!(
        last_of(&p1.frames(), 11).is_none(),
        "p3 may still come back: the question waits"
    );

    // p3 never does: after the allowance they no longer count
    adv(secs(6)).await;
    assert!(last_of(&p1.frames(), 11).is_some(), "the question closed");
}

#[tokio::test(start_paused = true)]
async fn a_restored_game_is_stored_again_unpaused_so_a_later_crash_does_not_push_the_clocks_again()
{
    let start = Instant::now();
    let w = world();
    let mut p = player(&w, 1).await;
    let h = host(&w).await;
    h.send(ClientFrame::Start(1)).await;
    h.send(ClientFrame::Next).await;
    settle().await;
    let _ = p.frames();
    w.deps.rooms.shutdown(secs(5)).await;
    assert!(
        Saved::read(&w.store.running.lock().unwrap()["s1"], BASE)
            .unwrap()
            .paused_at
            .is_some()
    );

    let down = wall_since(start) + 4_000;
    let w2 = restarted(&w, down).await;
    let row = w2.store.running.lock().unwrap()["s1"].clone();
    assert_eq!(
        Saved::read(&row, down).unwrap().paused_at,
        None,
        "stored again"
    );
    let mut first = connect(&w2, "s1", "p1", Role::Player).await;
    let deadline = last_of(&first.frames(), 2).unwrap()[4].clone();

    // the new process dies soon after: its state is as it was restored, not pushed again
    adv(secs(1)).await;
    let w3 = restarted(&w2, down + 1_000).await;
    let mut again = connect(&w3, "s1", "p1", Role::Player).await;
    assert_eq!(last_of(&again.frames(), 2).unwrap()[4], deadline);
}
