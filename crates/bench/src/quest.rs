//! Scenario `quest`: a class playing a quest game at human pace.
//!
//! N simulated players think for 6 to 10 seconds, answer (right seven times
//! in ten), pick a chest after a second or two, and go on; the host watches
//! the board. The report is what the game's budgets in the design are about:
//! bytes and frames per second per player, in each direction, what a player
//! sends per answer, the host's board rate, and the service's CPU and memory.

use super::{Args, Role, Secret, UNSET, Ws, client_wire, connect, mint, proc_stats, server_wire};
use anyhow::Context as _;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

/// Thinking time before an answer, in ms.
const THINK_MS: std::ops::Range<u64> = 6_000..10_000;
/// Time before opening a chest, in ms.
const CHEST_MS: std::ops::Range<u64> = 1_000..2_500;
/// Share of answers that are right, in percent.
const RIGHT_PERCENT: u64 = 70;

#[derive(Default)]
struct Stats {
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    frames_in: AtomicU64,
    frames_out: AtomicU64,
    answers: AtomicU64,
    right: AtomicU64,
    chests: AtomicU64,
    standings: AtomicU64,
    first_question: AtomicU64,
}

#[derive(Default)]
struct HostStats {
    bytes: AtomicU64,
    boards: AtomicU64,
    board_bytes_max: AtomicU64,
}

async fn host_task(stats: Arc<HostStats>, mut ws: futures::stream::SplitStream<Ws>) {
    while let Some(Ok(msg)) = ws.next().await {
        let Message::Text(text) = msg else { continue };
        stats
            .bytes
            .fetch_add(server_wire(text.len()), Ordering::Relaxed);
        if text.starts_with("[29,") {
            stats.boards.fetch_add(1, Ordering::Relaxed);
            stats
                .board_bytes_max
                .fetch_max(server_wire(text.len()), Ordering::Relaxed);
        }
    }
}

async fn player_task(stats: Arc<Stats>, mut ws: Ws, t0: Instant) {
    let mut answer_at: Option<(tokio::time::Instant, u64)> = None;
    let mut pick_at: Option<(tokio::time::Instant, u64)> = None;
    loop {
        let due = [answer_at.map(|a| a.0), pick_at.map(|p| p.0)]
            .into_iter()
            .flatten()
            .min();
        let msg = tokio::select! {
            m = ws.next() => m,
            _ = async { match due { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => {
                let now = tokio::time::Instant::now();
                let frame = if answer_at.is_some_and(|a| a.0 <= now) {
                    let (_, serial) = answer_at.take().expect("due");
                    stats.answers.fetch_add(1, Ordering::Relaxed);
                    let right = rand::random::<u64>() % 100 < RIGHT_PERCENT;
                    json!([6, serial, if right { 1 } else { 2 }]).to_string()
                } else {
                    let (_, serial) = pick_at.take().expect("due");
                    stats.chests.fetch_add(1, Ordering::Relaxed);
                    json!([7, serial, rand::random::<u64>() % 3]).to_string()
                };
                stats.bytes_out.fetch_add(client_wire(frame.len()), Ordering::Relaxed);
                stats.frames_out.fetch_add(1, Ordering::Relaxed);
                if ws.send(Message::text(frame)).await.is_err() {
                    return;
                }
                continue;
            }
        };
        let Some(Ok(Message::Text(text))) = msg else {
            match msg {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return,
            }
        };
        stats
            .bytes_in
            .fetch_add(server_wire(text.len()), Ordering::Relaxed);
        stats.frames_in.fetch_add(1, Ordering::Relaxed);
        let Ok(Value::Array(f)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match f.first().and_then(Value::as_u64) {
            Some(24) => {
                let serial = f[5].as_u64().unwrap_or(0);
                let think =
                    rand::random::<u64>() % (THINK_MS.end - THINK_MS.start) + THINK_MS.start;
                answer_at = Some((
                    tokio::time::Instant::now() + Duration::from_millis(think),
                    serial,
                ));
                let _ = stats.first_question.compare_exchange(
                    UNSET,
                    t0.elapsed().as_micros() as u64,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
            Some(25) => {
                if f[2].as_u64() == Some(1) {
                    stats.right.fetch_add(1, Ordering::Relaxed);
                    let wait =
                        rand::random::<u64>() % (CHEST_MS.end - CHEST_MS.start) + CHEST_MS.start;
                    pick_at = Some((
                        tokio::time::Instant::now() + Duration::from_millis(wait),
                        f[1].as_u64().unwrap_or(0),
                    ));
                }
            }
            Some(28) => {
                stats.standings.fetch_add(1, Ordering::Relaxed);
            }
            Some(6) | Some(7) => return,
            _ => {}
        }
    }
}

pub async fn run(args: Args, key: Secret) -> anyhow::Result<()> {
    let http = reqwest::Client::new();
    let run_id = format!("{:08x}", rand::random::<u32>());
    let classes = args.players.div_ceil(args.class_size).max(1);
    let sid_of = |c: usize| format!("bench-{run_id}-{c}");
    let teacher_of = |c: usize| format!("bench-teacher-{run_id}-{c}");
    let n = args.players;

    let idle = args.pid.and_then(proc_stats);
    // 24 questions of the typical size: enough that nobody sees a repeat in
    // three minutes at this pace
    let option = |c: char| c.to_string().repeat(args.option_chars);
    let questions: Vec<Value> = (0..24)
        .map(|i| {
            json!({
                "kind": "single",
                "text": format!("{i:02} ").to_string() + &"q".repeat(args.question_chars.saturating_sub(3)),
                "timeLimitSec": 30,
                "options": [
                    { "text": option('a'), "isCorrect": true }, { "text": option('b'), "isCorrect": false },
                    { "text": option('c'), "isCorrect": false }, { "text": option('d'), "isCorrect": false } ]
            })
        })
        .collect();
    let mut quiz_ids = Vec::new();
    for c in 0..classes {
        let t = mint(
            &key,
            Role::Teacher,
            &teacher_of(c),
            "Bench Teacher",
            &sid_of(c),
        )?;
        let created: Value = http
            .post(format!("{}/v1/quizzes", args.url))
            .bearer_auth(&t)
            .header("origin", &args.origin)
            .json(&json!({ "title": "Bench quest", "kept": false, "questions": questions }))
            .send()
            .await?
            .error_for_status()
            .context("creating the quiz")?
            .json()
            .await?;
        quiz_ids.push(created["id"].as_str().context("quiz id")?.to_string());
    }

    let t0 = Instant::now();
    let stats: Vec<Arc<Stats>> = (0..n)
        .map(|_| {
            let s = Stats::default();
            s.first_question.store(UNSET, Ordering::Relaxed);
            Arc::new(s)
        })
        .collect();
    let mut tasks = Vec::new();
    for wave in (0..n).collect::<Vec<_>>().chunks(100) {
        let mut pending = Vec::new();
        for &i in wave {
            let token = mint(
                &key,
                Role::Player,
                &format!("bench-student-{run_id}-{i:05}"),
                &format!("Student {i:04}"),
                &sid_of(i / args.class_size),
            )?;
            let args = &args;
            pending.push(async move { (i, connect(args, &token).await) });
        }
        for (i, ws) in futures::future::join_all(pending).await {
            tasks.push(tokio::spawn(player_task(stats[i].clone(), ws?, t0)));
        }
    }
    let host_stats = Arc::new(HostStats::default());
    let mut hosts = Vec::new();
    for c in 0..classes {
        let token = mint(
            &key,
            Role::Host,
            &teacher_of(c),
            "Bench Teacher",
            &sid_of(c),
        )?;
        let (sink, stream) = connect(&args, &token).await?.split();
        tokio::spawn(host_task(host_stats.clone(), stream));
        hosts.push(sink);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let connected = args.pid.and_then(proc_stats);

    for (host, quiz_id) in hosts.iter_mut().zip(&quiz_ids) {
        host.send(Message::text(json!([1, quiz_id, 1, 300, 0]).to_string()))
            .await?;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    for host in &mut hosts {
        host.send(Message::text(json!([2]).to_string())).await?;
    }
    // let the first questions land, then measure a steady window
    tokio::time::sleep(Duration::from_secs(10)).await;
    let sum = |f: &dyn Fn(&Stats) -> u64| stats.iter().map(|s| f(s)).sum::<u64>();
    let standings_start = sum(&|s| s.standings.load(Ordering::Relaxed));
    let start = (
        Instant::now(),
        sum(&|s| s.bytes_in.load(Ordering::Relaxed)),
        sum(&|s| s.bytes_out.load(Ordering::Relaxed)),
        sum(&|s| s.frames_in.load(Ordering::Relaxed)),
        sum(&|s| s.frames_out.load(Ordering::Relaxed)),
        sum(&|s| s.answers.load(Ordering::Relaxed)),
        host_stats.boards.load(Ordering::Relaxed),
        host_stats.bytes.load(Ordering::Relaxed),
        args.pid.and_then(proc_stats),
    );
    tokio::time::sleep(Duration::from_secs(args.duration)).await;
    let standings_end = sum(&|s| s.standings.load(Ordering::Relaxed));
    let end = (
        Instant::now(),
        sum(&|s| s.bytes_in.load(Ordering::Relaxed)),
        sum(&|s| s.bytes_out.load(Ordering::Relaxed)),
        sum(&|s| s.frames_in.load(Ordering::Relaxed)),
        sum(&|s| s.frames_out.load(Ordering::Relaxed)),
        sum(&|s| s.answers.load(Ordering::Relaxed)),
        host_stats.boards.load(Ordering::Relaxed),
        host_stats.bytes.load(Ordering::Relaxed),
        args.pid.and_then(proc_stats),
    );
    let secs = end.0.duration_since(start.0).as_secs_f64();
    let per_player = |a: u64, b: u64| (b - a) as f64 / n as f64 / secs;
    let answers = (end.5 - start.5).max(1);
    for host in &mut hosts {
        host.send(Message::text(json!([3]).to_string())).await?;
    }
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
    }

    println!(
        "quiz-bench quest: {n} players in {classes} class{} playing at once, steady window of {secs:.0} s, answers every {}-{} s",
        if classes == 1 { "" } else { "es" },
        THINK_MS.start / 1000,
        THINK_MS.end / 1000
    );
    println!("per player:");
    println!(
        "  bytes received per second   : {:.0} B   (budget 400)",
        per_player(start.1, end.1)
    );
    println!(
        "  frames received per second  : {:.2}   (budget 1.2)",
        per_player(start.3, end.3)
    );
    println!(
        "  bytes sent per second       : {:.1} B",
        per_player(start.2, end.2)
    );
    println!(
        "  bytes sent per answer       : {:.1} B   (budget 30; an answer and, when right, a chest pick)",
        (end.2 - start.2) as f64 / answers as f64
    );
    println!(
        "  frames sent per second      : {:.2}",
        per_player(start.4, end.4)
    );
    println!(
        "answers in the window: {answers} ({:.1} per second for the class), right {:.0}%",
        answers as f64 / secs,
        100.0 * sum(&|s| s.right.load(Ordering::Relaxed)) as f64
            / sum(&|s| s.answers.load(Ordering::Relaxed)).max(1) as f64
    );
    println!(
        "standings strips received: {:.2} per player per second (at most one every 3 s)",
        per_player(standings_start, standings_end)
    );
    println!(
        "host: {:.2} board frames per second (budget 2), biggest {} B, {:.0} B/s",
        (end.6 - start.6) as f64 / secs / classes as f64,
        host_stats.board_bytes_max.load(Ordering::Relaxed),
        (end.7 - start.7) as f64 / secs / classes as f64
    );
    if let (Some(a), Some(b)) = (start.8, end.8) {
        println!(
            "service CPU: {:.1}% of one core over the window (budget 5%)",
            (b.1 - a.1) / secs * 100.0
        );
    }
    if let (Some(idle), Some(connected), Some(done)) = (idle, connected, end.8) {
        println!(
            "memory: idle RSS {:.1} MiB, {n} idle sockets {:.1} MiB, during the game {:.1} MiB",
            idle.0 as f64 / 1024.0,
            connected.0 as f64 / 1024.0,
            done.0 as f64 / 1024.0
        );
    }
    Ok(())
}
