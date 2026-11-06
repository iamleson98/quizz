//! quiz-bench: a class playing one quiz against a running quiz-service.
//!
//! Scenario `game`: N players and a host connect to one session over real
//! websockets; the host opens a lobby and runs the questions; every player
//! answers within a spread; the bench reports what the service's budgets are
//! about: bytes and frames per player per question, how long the whole class
//! takes to receive a question, how long a question takes to close once the
//! last answer is in, memory per socket, CPU per question and the writes the
//! service made.
//!
//! The service must run with `QUIZ_TICKET_KEY` / `QUIZ_ALLOWED_ORIGINS` the
//! bench is given (`--origin`, env `QUIZ_TICKET_KEY`). Raise `ulimit -n` for
//! a thousand players.
//!
//! ```text
//! QUIZ_TICKET_KEY=... quiz-bench --url http://127.0.0.1:8080 \
//!     --internal-url http://127.0.0.1:8081 --origin https://site.example \
//!     --players 300 --questions 5 --pid <service pid>
//! ```

use anyhow::{Context as _, bail};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use quiz_service::auth::{Role, Ticket};
use quiz_service::config::Secret;
use quiz_service::game::frames;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "quiz-bench",
    about = "A class playing a quiz against quiz-service"
)]
struct Args {
    /// Public listener of the service.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    url: String,

    /// Internal listener (metrics).
    #[arg(long, default_value = "http://127.0.0.1:8081")]
    internal_url: String,

    /// An origin the service allows.
    #[arg(long, default_value = "https://site.example")]
    origin: String,

    /// Scenario: only `game` exists.
    #[arg(long, default_value = "game")]
    scenario: String,

    /// Players in the class.
    #[arg(long, default_value_t = 300)]
    players: usize,

    /// Players per session: a game takes at most 500, so more players are
    /// spread over several sessions (each with its own host and game), all
    /// playing at once.
    #[arg(long, default_value_t = 500)]
    class_size: usize,

    /// Questions to play.
    #[arg(long, default_value_t = 5)]
    questions: usize,

    /// Every player answers within this many ms of the question opening.
    #[arg(long, default_value_t = 2000)]
    spread_ms: u64,

    /// Characters of a question's text.
    #[arg(long, default_value_t = 80)]
    question_chars: usize,

    /// Characters of each of the four options.
    #[arg(long, default_value_t = 30)]
    option_chars: usize,

    /// Characters of the explanation shown at the reveal (0: none).
    #[arg(long, default_value_t = 0)]
    explanation_chars: usize,

    /// Process id of the service, to read its memory and CPU time.
    #[arg(long)]
    pid: Option<u32>,

    /// Print the first question and reveal frames a player received.
    #[arg(long, default_value_t = false)]
    show_frames: bool,
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn main() {
    let args = Args::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(e) = rt.block_on(run(args)) {
        eprintln!("quiz-bench failed: {e:#}");
        std::process::exit(1);
    }
}

const UNSET: u64 = u64::MAX;

/// What one player's socket saw.
struct Player {
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    frames_in: AtomicU64,
    frames_out: AtomicU64,
    /// Micros since the bench started, per question; UNSET until it happens.
    question_at: Vec<AtomicU64>,
    answered_at: Vec<AtomicU64>,
    reveal_at: Vec<AtomicU64>,
    lobby: AtomicU64,
    sample: std::sync::Mutex<Vec<String>>,
}

struct Shared {
    t0: Instant,
    players: Vec<Player>,
}

impl Shared {
    fn micros(&self) -> u64 {
        self.t0.elapsed().as_micros() as u64
    }

    fn sum(&self, f: impl Fn(&Player) -> u64) -> u64 {
        self.players.iter().map(f).sum()
    }

    /// Wait until `count` players satisfy `done`, up to 60 s.
    async fn wait(&self, what: &str, done: impl Fn(&Player) -> bool) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if self.players.iter().all(&done) {
                return Ok(());
            }
            if Instant::now() > deadline {
                let missing = self.players.iter().filter(|p| !done(p)).count();
                bail!(
                    "timed out waiting for {what}: {missing} of {} players still missing",
                    self.players.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn mint(key: &Secret, role: Role, sub: &str, name: &str, sid: &str) -> anyhow::Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let in_room = role != Role::Teacher;
    Ticket {
        sub: sub.into(),
        name: name.into(),
        role,
        sid: in_room.then(|| sid.to_string()),
        cid: in_room.then(|| "bench-class".to_string()),
        rc: in_room.then(|| now * 1000 + 3 * 60 * 60 * 1000),
    }
    .sign(key, now, now + 600)
    .map_err(|e| anyhow::anyhow!("{e}"))
}

async fn connect(args: &Args, token: &str) -> anyhow::Result<Ws> {
    let ws_url = args.url.replacen("http", "ws", 1) + "/ws";
    let mut req = ws_url.into_client_request()?;
    req.headers_mut()
        .insert("origin", HeaderValue::from_str(&args.origin)?);
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_static("quiz.v1"),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("websocket upgrade")?;
    ws.send(Message::text(json!([0, token]).to_string()))
        .await?;
    Ok(ws)
}

/// A client frame's size on the wire: payload plus its 2 header bytes and
/// the 4-byte mask every client frame carries.
fn client_wire(len: usize) -> u64 {
    (len + 6 + if len > 125 { 2 } else { 0 }) as u64
}

/// A server frame's size on the wire: payload plus its header.
fn server_wire(len: usize) -> u64 {
    (len + 2 + if len > 125 { 2 } else { 0 } + if len > 65535 { 6 } else { 0 }) as u64
}

/// What a host's socket received: bytes and frames, and the biggest `names`
/// frame (type 10).
#[derive(Default)]
struct HostStats {
    bytes: AtomicU64,
    frames: AtomicU64,
    names_bytes: AtomicU64,
    progress_frames: AtomicU64,
    progress_bytes_max: AtomicU64,
}

async fn host_task(stats: Arc<HostStats>, mut ws: futures::stream::SplitStream<Ws>) {
    while let Some(msg) = ws.next().await {
        let Ok(Message::Text(text)) = msg else {
            match msg {
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                _ => break,
            }
        };
        stats
            .bytes
            .fetch_add(server_wire(text.len()), Ordering::Relaxed);
        stats.frames.fetch_add(1, Ordering::Relaxed);
        match text.as_bytes().get(1..3) {
            Some(b"10") => {
                stats
                    .names_bytes
                    .fetch_max(server_wire(text.len()), Ordering::Relaxed);
            }
            Some(b"4,") => {
                stats.progress_frames.fetch_add(1, Ordering::Relaxed);
                stats
                    .progress_bytes_max
                    .fetch_max(server_wire(text.len()), Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

async fn player_task(shared: Arc<Shared>, idx: usize, mut ws: Ws, spread_ms: u64, show: bool) {
    let me = &shared.players[idx];
    while let Some(msg) = ws.next().await {
        let Ok(Message::Text(text)) = msg else {
            match msg {
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                _ => break,
            }
        };
        me.bytes_in
            .fetch_add(server_wire(text.len()), Ordering::Relaxed);
        me.frames_in.fetch_add(1, Ordering::Relaxed);
        let Ok(Value::Array(frame)) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match frame.first().and_then(Value::as_u64) {
            Some(1) => me.lobby.store(shared.micros(), Ordering::Relaxed),
            Some(2) => {
                let q = frame[2].as_u64().unwrap_or(1) as usize;
                if q == 0 || q > me.question_at.len() {
                    continue;
                }
                me.question_at[q - 1].store(shared.micros(), Ordering::Relaxed);
                if show && idx == 0 {
                    me.sample.lock().unwrap().push(text.to_string());
                }
                let delay = if spread_ms == 0 {
                    0
                } else {
                    rand::random::<u64>() % spread_ms
                };
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let mask = if rand::random::<bool>() { 1 } else { 2 };
                let answer = json!([4, q, mask]).to_string();
                me.bytes_out
                    .fetch_add(client_wire(answer.len()), Ordering::Relaxed);
                me.frames_out.fetch_add(1, Ordering::Relaxed);
                me.answered_at[q - 1].store(shared.micros(), Ordering::Relaxed);
                if ws.send(Message::text(answer)).await.is_err() {
                    break;
                }
            }
            Some(5) => {
                let q = frame[1].as_u64().unwrap_or(1) as usize;
                if q >= 1 && q <= me.reveal_at.len() {
                    me.reveal_at[q - 1].store(shared.micros(), Ordering::Relaxed);
                    if show && idx == 0 {
                        me.sample.lock().unwrap().push(text.to_string());
                    }
                }
            }
            Some(7) => break,
            _ => {}
        }
    }
}

/// `(rss KiB, cpu seconds)` of a process, read with `ps`.
fn proc_stats(pid: u32) -> Option<(u64, f64)> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=,cputime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let rss = it.next()?.parse().ok()?;
    let cpu = it.next()?;
    // [[dd-]hh:]mm:ss[.ss]
    let secs = cpu
        .rsplit(':')
        .enumerate()
        .map(|(i, part)| part.parse::<f64>().unwrap_or(0.0) * 60f64.powi(i as i32))
        .sum::<f64>();
    Some((rss, secs))
}

async fn metrics_text(client: &reqwest::Client, args: &Args) -> String {
    match client
        .get(format!("{}/metrics", args.internal_url))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// Sum of a metric's samples (all label sets).
fn metric(text: &str, name: &str) -> f64 {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter(|l| {
            l.starts_with(name) && matches!(l.as_bytes().get(name.len()), Some(b' ' | b'{'))
        })
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

async fn run(args: Args) -> anyhow::Result<()> {
    if args.scenario != "game" {
        bail!(
            "unknown scenario `{}`: the only one is `game`",
            args.scenario
        );
    }
    let key = Secret::new(
        std::env::var("QUIZ_TICKET_KEY")
            .context("QUIZ_TICKET_KEY must be set (the service's ticket key)")?,
    );
    let http = reqwest::Client::new();
    let run_id = format!("{:08x}", rand::random::<u32>());
    let classes = args.players.div_ceil(args.class_size).max(1);
    let sid_of = |c: usize| format!("bench-{run_id}-{c}");
    let teacher_of = |c: usize| format!("bench-teacher-{run_id}-{c}");

    let idle = args.pid.and_then(proc_stats);
    let metrics_before = metrics_text(&http, &args).await;

    // The quiz: questions of the typical size, correct option 1 first.
    let option = |c: char| c.to_string().repeat(args.option_chars);
    let questions: Vec<Value> = (0..args.questions)
        .map(|i| {
            let mut q = json!({
                "kind": "single",
                "text": format!("{i:02} ").to_string() + &"q".repeat(args.question_chars.saturating_sub(3)),
                "timeLimitSec": 120,
                "options": [
                    { "text": option('a'), "isCorrect": true }, { "text": option('b'), "isCorrect": false },
                    { "text": option('c'), "isCorrect": false }, { "text": option('d'), "isCorrect": false } ]
            });
            if args.explanation_chars > 0 {
                q["explanation"] = json!("e".repeat(args.explanation_chars));
            }
            q
        })
        .collect();
    let mut quiz_ids = Vec::new();
    for c in 0..classes {
        let teacher_ticket = mint(
            &key,
            Role::Teacher,
            &teacher_of(c),
            "Bench Teacher",
            &sid_of(c),
        )?;
        let created: Value = http
            .post(format!("{}/v1/quizzes", args.url))
            .bearer_auth(&teacher_ticket)
            .header("origin", &args.origin)
            .json(&json!({ "title": "Bench quiz", "kept": false, "questions": questions }))
            .send()
            .await?
            .error_for_status()
            .context("creating the quiz")?
            .json()
            .await?;
        quiz_ids.push(created["id"].as_str().context("quiz id")?.to_string());
    }

    let n = args.players;
    let q = args.questions;
    let make = |_| Player {
        bytes_in: AtomicU64::new(0),
        bytes_out: AtomicU64::new(0),
        frames_in: AtomicU64::new(0),
        frames_out: AtomicU64::new(0),
        question_at: (0..q).map(|_| AtomicU64::new(UNSET)).collect(),
        answered_at: (0..q).map(|_| AtomicU64::new(UNSET)).collect(),
        reveal_at: (0..q).map(|_| AtomicU64::new(UNSET)).collect(),
        lobby: AtomicU64::new(UNSET),
        sample: Default::default(),
    };
    let shared = Arc::new(Shared {
        t0: Instant::now(),
        players: (0..n).map(make).collect(),
    });

    // Everyone connects (in waves, so the listen queue is not the test).
    let connect_started = Instant::now();
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
            let (sh, spread, show) = (shared.clone(), args.spread_ms, args.show_frames);
            tasks.push(tokio::spawn(player_task(sh, i, ws?, spread, show)));
        }
    }
    let host_stats = Arc::new(HostStats::default());
    let mut hosts: Vec<futures::stream::SplitSink<Ws, Message>> = Vec::new();
    for c in 0..classes {
        let host_token = mint(
            &key,
            Role::Host,
            &teacher_of(c),
            "Bench Teacher",
            &sid_of(c),
        )?;
        let (sink, stream) = connect(&args, &host_token).await?.split();
        tokio::spawn(host_task(host_stats.clone(), stream));
        hosts.push(sink);
    }
    // let every hello settle: each socket gets its first (state) frame
    shared
        .wait("every player's first frame", |p| {
            p.frames_in.load(Ordering::Relaxed) >= 1
        })
        .await?;
    let connect_secs = connect_started.elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let connected = args.pid.and_then(proc_stats);

    // Lobby.
    for (host, quiz_id) in hosts.iter_mut().zip(&quiz_ids) {
        host.send(Message::text(json!([1, quiz_id]).to_string()))
            .await?;
    }
    shared
        .wait("the lobby", |p| p.lobby.load(Ordering::Relaxed) != UNSET)
        .await?;
    // A host that says hello once the lobby is full gets every name in ONE
    // frame: measure its size.
    let mut late_host = connect(
        &args,
        &mint(
            &key,
            Role::Host,
            &teacher_of(0),
            "Bench Teacher",
            &sid_of(0),
        )?,
    )
    .await?;
    let mut full_names_bytes = 0;
    while let Ok(Some(Ok(Message::Text(t)))) =
        tokio::time::timeout(Duration::from_millis(500), late_host.next()).await
    {
        if t.starts_with("[10,") {
            full_names_bytes = server_wire(t.len());
        }
    }
    drop(late_host);
    let cpu_game_start = args.pid.and_then(proc_stats);

    struct Round {
        reach: Vec<u64>,
        close_after_last_answer: u64,
        bytes_in: f64,
        bytes_out: f64,
        frames_in: f64,
    }
    let mut rounds = Vec::new();
    for qi in 0..q {
        let (bytes_in0, bytes_out0, frames_in0) = (
            shared.sum(|p| p.bytes_in.load(Ordering::Relaxed)),
            shared.sum(|p| p.bytes_out.load(Ordering::Relaxed)),
            shared.sum(|p| p.frames_in.load(Ordering::Relaxed)),
        );
        let opened = shared.micros();
        for host in &mut hosts {
            host.send(Message::text(json!([2]).to_string())).await?;
        }
        shared
            .wait("the question", |p| {
                p.question_at[qi].load(Ordering::Relaxed) != UNSET
            })
            .await?;
        shared
            .wait("every answer", |p| {
                p.answered_at[qi].load(Ordering::Relaxed) != UNSET
            })
            .await?;
        // everyone answered: the service closes the question by itself
        shared
            .wait("the reveal", |p| {
                p.reveal_at[qi].load(Ordering::Relaxed) != UNSET
            })
            .await?;
        let last_answer = shared
            .players
            .iter()
            .map(|p| p.answered_at[qi].load(Ordering::Relaxed))
            .max()
            .unwrap_or(0);
        let last_reveal = shared
            .players
            .iter()
            .map(|p| p.reveal_at[qi].load(Ordering::Relaxed))
            .max()
            .unwrap_or(0);
        let mut reach: Vec<u64> = shared
            .players
            .iter()
            .map(|p| {
                p.question_at[qi]
                    .load(Ordering::Relaxed)
                    .saturating_sub(opened)
            })
            .collect();
        reach.sort_unstable();
        // let the acks settle before reading the byte counters
        tokio::time::sleep(Duration::from_millis(100)).await;
        rounds.push(Round {
            reach,
            close_after_last_answer: last_reveal.saturating_sub(last_answer),
            bytes_in: (shared.sum(|p| p.bytes_in.load(Ordering::Relaxed)) - bytes_in0) as f64
                / n as f64,
            bytes_out: (shared.sum(|p| p.bytes_out.load(Ordering::Relaxed)) - bytes_out0) as f64
                / n as f64,
            frames_in: (shared.sum(|p| p.frames_in.load(Ordering::Relaxed)) - frames_in0) as f64
                / n as f64,
        });
    }
    let cpu_game_end = args.pid.and_then(proc_stats);

    // podium, end
    for host in &mut hosts {
        host.send(Message::text(json!([2]).to_string())).await?;
        host.send(Message::text(json!([3]).to_string())).await?;
    }
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(10), t).await;
    }
    let metrics_after = metrics_text(&http, &args).await;

    // ---- report
    let mean =
        |f: &dyn Fn(&Round) -> f64| rounds.iter().map(f).sum::<f64>() / rounds.len().max(1) as f64;
    let mut reach_all: Vec<u64> = rounds
        .iter()
        .flat_map(|r| r.reach.iter().copied())
        .collect();
    reach_all.sort_unstable();
    let worst_reach = rounds
        .iter()
        .map(|r| *r.reach.last().unwrap_or(&0))
        .max()
        .unwrap_or(0);
    let close_worst = rounds
        .iter()
        .map(|r| r.close_after_last_answer)
        .max()
        .unwrap_or(0);
    println!(
        "quiz-bench game: {n} players in {classes} class{} playing at once, {q} questions, answers spread over {} ms",
        if classes == 1 { "" } else { "es" },
        args.spread_ms
    );
    println!(
        "  question: {} chars, four options of {} chars{}",
        args.question_chars,
        args.option_chars,
        if args.explanation_chars > 0 {
            format!(", explanation {} chars", args.explanation_chars)
        } else {
            String::new()
        }
    );
    println!("  connecting {n} sockets took {connect_secs:.2} s");
    println!("per player per question:");
    println!(
        "  bytes received from the service : {:.0} B   (budget 1024)",
        mean(&|r| r.bytes_in)
    );
    println!(
        "  bytes sent to the service       : {:.0} B   (budget 64)",
        mean(&|r| r.bytes_out)
    );
    println!(
        "  frames received                 : {:.2}   (question, ack, reveal)",
        mean(&|r| r.frames_in)
    );
    println!("whole class reached after a question opens:");
    println!(
        "  p50 {:.1} ms   p99 {:.1} ms   slowest {:.1} ms   (budget 100 ms)",
        percentile(&reach_all, 0.5) as f64 / 1000.0,
        percentile(&reach_all, 0.99) as f64 / 1000.0,
        worst_reach as f64 / 1000.0
    );
    println!(
        "question closed after the last answer: slowest {:.1} ms (all players saw the reveal)",
        close_worst as f64 / 1000.0
    );

    let counter = |name: &str| metric(&metrics_after, name) - metric(&metrics_before, name);
    let closes = counter("quiz_question_close_seconds_count");
    if closes > 0.0 {
        println!(
            "service: question close (reveal + one write) mean {:.1} ms over {closes:.0} questions",
            counter("quiz_question_close_seconds_sum") / closes * 1000.0
        );
    }
    println!(
        "service: write transactions {:.0}, conflict retries {:.0}",
        counter("tx_commits_total"),
        counter("tx_retries_total")
    );
    let first_class = n.min(args.class_size);
    println!(
        "host (per class): {:.0} B and {:.1} frames per question; the one full-names frame for {first_class} players: {full_names_bytes} B",
        host_stats.bytes.load(Ordering::Relaxed) as f64 / (classes * q) as f64,
        host_stats.frames.load(Ordering::Relaxed) as f64 / (classes * q) as f64,
    );
    println!(
        "host progress frames (counts and provisional top ten): {} over {} questions of {classes} class(es), the biggest {} B",
        host_stats.progress_frames.load(Ordering::Relaxed),
        q,
        host_stats.progress_bytes_max.load(Ordering::Relaxed)
    );
    if let (Some(idle), Some(connected)) = (idle, connected) {
        println!(
            "memory: idle RSS {:.1} MiB, with {n} idle sockets {:.1} MiB -> {:.1} KiB per socket (budget 20)",
            idle.0 as f64 / 1024.0,
            connected.0 as f64 / 1024.0,
            (connected.0.saturating_sub(idle.0)) as f64 / n as f64
        );
    }
    if let (Some(a), Some(b)) = (cpu_game_start, cpu_game_end) {
        println!(
            "cpu: {:.1} ms of service CPU per question (whole class)",
            (b.1 - a.1) * 1000.0 / q as f64
        );
    }
    // What sharing saves: encoding the question once instead of per socket.
    let options: Vec<String> = ["a", "b", "c", "d"]
        .iter()
        .map(|c| c.repeat(args.option_chars))
        .collect();
    let refs: Vec<&str> = options.iter().map(String::as_str).collect();
    let text = "q".repeat(args.question_chars);
    let started = Instant::now();
    let rounds_n = 2000;
    for i in 0..rounds_n {
        let head = frames::question_head(
            1_800_000_000_000 + i,
            1,
            q,
            1_800_000_020_000,
            0,
            &text,
            &refs,
        );
        std::hint::black_box(frames::close(head));
    }
    let encode = started.elapsed().as_secs_f64() / rounds_n as f64;
    println!(
        "encoding: one question frame {:.1} us; once for the class {:.2} ms, per socket {:.2} ms",
        encode * 1e6,
        encode * 1e3,
        encode * n as f64 * 1e3
    );

    if args.show_frames {
        for f in shared.players[0].sample.lock().unwrap().iter().take(2) {
            println!("sample frame ({} bytes): {f}", f.len());
        }
    }
    Ok(())
}
