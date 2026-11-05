//! quiz-bench: a load generator for the quiz-service GraphQL API.
//!
//! Scenarios:
//!   read  - leaderboard / quiz detail / quiz list queries
//!   write - startAttempt + submitAttempt cycles (BEGIN CONCURRENT hot path)
//!   mixed - 80% read / 20% write (default)
//!
//! Example:
//!   quiz-bench --url http://127.0.0.1:8080 --scenario write \
//!              --concurrency 16 --duration 30

use anyhow::Context as _;
use clap::Parser;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Parser, Debug, Clone)]
#[command(name = "quiz-bench", about = "Load generator for quiz-service")]
struct Args {
    /// Base URL of the service.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    url: String,

    /// Test duration in seconds.
    #[arg(long, default_value = "15")]
    duration: u64,

    /// Concurrent workers.
    #[arg(long, default_value = "8")]
    concurrency: usize,

    /// Scenario: read | write | mixed.
    #[arg(long, default_value = "mixed")]
    scenario: String,

    /// Players to bootstrap (>= concurrency recommended).
    #[arg(long, default_value = "16")]
    users: usize,

    /// Questions in each bench quiz.
    #[arg(long, default_value = "10")]
    questions: usize,

    /// Number of quizzes to spread writes across (contention profile).
    #[arg(long, default_value = "1")]
    quizzes: usize,

    /// Skip bootstrap (reuse previously created bench users/quiz).
    #[arg(long, default_value_t = false)]
    no_bootstrap: bool,
}

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

struct Client {
    http: reqwest::Client,
    endpoint: String,
}

impl Client {
    fn new(url: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(64)
                .tcp_nodelay(true)
                .build()
                .expect("build http client"),
            endpoint: format!("{url}/graphql"),
        }
    }

    /// Execute a GraphQL operation; returns (response, elapsed micros).
    async fn gql(
        &self,
        token: Option<&str>,
        query: &str,
        variables: Value,
    ) -> anyhow::Result<(Value, u64)> {
        let started = Instant::now();
        let mut req = self.http.post(&self.endpoint).json(&json!({
            "query": query, "variables": variables
        }));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        anyhow::ensure!(status.is_success(), "http {status}: {body}");
        Ok((body, started.elapsed().as_micros() as u64))
    }
}

/// One recorded request.
#[derive(Clone)]
struct Record {
    op: &'static str,
    micros: u64,
    ok: bool,
}

#[derive(Default)]
struct Stats {
    records: Mutex<Vec<Record>>,
}

impl Stats {
    fn push(&self, op: &'static str, micros: u64, ok: bool) {
        self.records.lock().unwrap().push(Record { op, micros, ok });
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn fmt_us(v: u64) -> String {
    if v >= 1000 {
        format!("{:.2} ms", v as f64 / 1000.0)
    } else {
        format!("{v} µs")
    }
}

struct QuizData {
    quiz_id: String,
    questions: Vec<(String, Vec<String>)>, // (question id, option ids)
}

struct Bench {
    quizzes: Vec<QuizData>,
}

async fn bootstrap(args: &Args, client: &Client) -> anyhow::Result<(Vec<String>, Bench)> {
    eprintln!(
        "bootstrapping: {} users, {} questions ...",
        args.users, args.questions
    );

    // health check - and show which SQLite the server runs.
    let health: Value = client
        .http
        .get(format!("{}/healthz", args.url))
        .send()
        .await?
        .json()
        .await?;
    eprintln!("server: {} (sqlite {})", args.url, health["sqlite"]);

    // register users (parallel batches; server-side argon2 is the slow part)
    let n = args.users;
    let mut tokens = Vec::with_capacity(n);
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    for chunk in (0..n).collect::<Vec<_>>().chunks(8) {
        let mut futs = Vec::new();
        for i in chunk {
            let http = client.http.clone();
            let endpoint = client.endpoint.clone();
            let username = format!("bench_u{uniq}_{i}");
            futs.push(async move {
                let resp: Value = http
                    .post(endpoint)
                    .json(&json!({
                        "query": "mutation($u: String!, $p: String!) { register(username: $u, password: $p) { token } }",
                        "variables": { "u": username, "p": "benchpassword1" }
                    }))
                    .send()
                    .await?
                    .json()
                    .await?;
                anyhow::Ok(resp["data"]["register"]["token"].as_str().context("no token")?.to_string())
            });
        }
        for f in futs {
            tokens.push(f.await?);
        }
    }

    // author = user 0; create N quizzes with their questions
    let author = &tokens[0];
    let mut quizzes = Vec::new();
    for _qn in 0..args.quizzes {
        let quiz: Value = client
            .gql(
                Some(author),
                r#"mutation { createQuiz(input: { title: "Bench Quiz" }) { id } }"#,
                json!({}),
            )
            .await?
            .0;
        let quiz_id = quiz["data"]["createQuiz"]["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("createQuiz failed: {quiz}"))?
            .to_string();

        // build questions: alternate single / multi / boolean
        let mut input = String::from("[");
        for i in 0..args.questions {
            if i > 0 {
                input.push(',');
            }
            let (kind, options): (&str, &str) = match i % 3 {
                0 => (
                    "SINGLE",
                    r#"[{text: "a", isCorrect: true}, {text: "b", isCorrect: false}, {text: "c", isCorrect: false}]"#,
                ),
                1 => (
                    "MULTI",
                    r#"[{text: "a", isCorrect: true}, {text: "b", isCorrect: false}, {text: "c", isCorrect: true}]"#,
                ),
                _ => (
                    "BOOLEAN",
                    r#"[{text: "True", isCorrect: true}, {text: "False", isCorrect: false}]"#,
                ),
            };
            input.push_str(&format!(
                r#"{{ kind: {kind}, text: "q{i}", points: 1.0, options: {options} }}"#
            ));
        }
        input.push(']');
        let addq =
            format!(r#"mutation($q: ID!) {{ addQuestions(quizId: $q, input: {input}) {{ id }} }}"#);
        let res = client
            .gql(Some(author), &addq, json!({ "q": quiz_id }))
            .await?
            .0;
        anyhow::ensure!(res["errors"].is_null(), "addQuestions failed: {res}");

        // fetch question/option ids
        let detail = client
            .gql(
                Some(author),
                r#"query($id: ID!) { quiz(id: $id) { questions { id options { id } } } }"#,
                json!({ "id": quiz_id }),
            )
            .await?
            .0;
        let questions: Vec<(String, Vec<String>)> = detail["data"]["quiz"]["questions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| {
                (
                    q["id"].as_str().unwrap().to_string(),
                    q["options"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|o| o["id"].as_str().unwrap().to_string())
                        .collect(),
                )
            })
            .collect();
        anyhow::ensure!(!questions.is_empty(), "no questions loaded");
        quizzes.push(QuizData { quiz_id, questions });
    } // for each quiz

    eprintln!(
        "bootstrap done: {} quiz(zes) x {} questions",
        quizzes.len(),
        quizzes[0].questions.len()
    );
    Ok((tokens, Bench { quizzes }))
}

async fn read_op(client: &Client, bench: &Bench, stats: &Stats) {
    let pick = rand::random_range(0u8..100);
    let quiz_id = &bench.quizzes[0].quiz_id;
    let (op, query, vars): (&'static str, &str, Value) = if pick < 50 {
        (
            "leaderboard",
            r#"query($q: ID!) { leaderboard(quizId: $q, first: 10) { rank user { username } score } }"#,
            json!({ "q": quiz_id }),
        )
    } else if pick < 90 {
        (
            "quizDetail",
            r#"query($id: ID!) { quiz(id: $id) { quiz { title questionCount } questions { id options { id text } } } }"#,
            json!({ "id": quiz_id }),
        )
    } else {
        (
            "quizList",
            r#"{ quizzes(first: 20) { total items { id title } } }"#,
            json!({}),
        )
    };

    match client.gql(None, query, vars).await {
        Ok((res, us)) => {
            let ok = res["errors"].is_null() && !res["data"].is_null();
            stats.push(op, us, ok);
        }
        Err(_) => stats.push(op, 0, false),
    }
}

async fn write_op(client: &Client, token: &str, bench: &Bench, stats: &Stats) {
    // spread writes across quizzes (random pick)
    let data = &bench.quizzes[rand::random_range(0..bench.quizzes.len())];

    // 1) start attempt
    let start = client
        .gql(
            Some(token),
            r#"mutation($q: ID!) { startAttempt(quizId: $q) { id } }"#,
            json!({ "q": data.quiz_id }),
        )
        .await;
    let attempt = match start {
        Ok((res, us)) => {
            let ok = res["errors"].is_null();
            stats.push("startAttempt", us, ok);
            match res["data"]["startAttempt"]["id"].as_str() {
                Some(id) if ok => id.to_string(),
                _ => return,
            }
        }
        Err(_) => {
            stats.push("startAttempt", 0, false);
            return;
        }
    };

    // 2) random answers
    let answers: Vec<Value> = data
        .questions
        .iter()
        .map(|(qid, opts)| {
            let picks: Vec<String> = opts
                .iter()
                .filter(|_| rand::random_bool(0.5))
                .cloned()
                .collect();
            // ensure at least one selection
            let picks = if picks.is_empty() {
                vec![opts[rand::random_range(0..opts.len())].clone()]
            } else {
                picks
            };
            json!({ "questionId": qid, "selectedOptionIds": picks })
        })
        .collect();

    let submit = client
        .gql(
            Some(token),
            r#"mutation($a: ID!, $ans: [AnswerInput!]!) {
                submitAttempt(attemptId: $a, answers: $ans) { score maxScore }
            }"#,
            json!({ "a": attempt, "ans": answers }),
        )
        .await;
    match submit {
        Ok((res, us)) => stats.push("submitAttempt", us, res["errors"].is_null()),
        Err(_) => stats.push("submitAttempt", 0, false),
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    let client = Arc::new(Client::new(&args.url));
    let (tokens, bench) = if args.no_bootstrap {
        anyhow::bail!("--no-bootstrap requires persistent state; not implemented");
    } else {
        bootstrap(&args, &client).await?
    };
    let bench = Arc::new(bench);

    let scenario = args.scenario.clone();
    let duration = Duration::from_secs(args.duration);
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());

    eprintln!(
        "running scenario={scenario} concurrency={} duration={}s ...",
        args.concurrency, args.duration
    );
    let started = Instant::now();

    let mut handles = Vec::new();
    for w in 0..args.concurrency {
        let client = client.clone();
        let bench = bench.clone();
        let stop = stop.clone();
        let stats = stats.clone();
        let scenario = scenario.clone();
        let token = tokens[w % tokens.len()].clone();
        handles.push(tokio::spawn(async move {
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let do_write = match scenario.as_str() {
                    "read" => false,
                    "write" => true,
                    _ => rand::random_bool(0.2),
                };
                if do_write {
                    write_op(&client, &token, &bench, &stats).await;
                } else {
                    read_op(&client, &bench, &stats).await;
                }
            }
        }));
    }

    tokio::time::sleep(duration).await;
    stop.store(true, Ordering::SeqCst);
    for h in handles {
        let _ = h.await;
    }
    let elapsed = started.elapsed();

    // ------------------------------------------------------------ report
    let records = std::mem::take(&mut *stats.records.lock().unwrap());
    let total = records.len();
    let errors = records.iter().filter(|r| !r.ok).count();
    let mut by_op: HashMap<&str, (usize, usize, Vec<u64>)> = HashMap::new();
    let mut all: Vec<u64> = Vec::with_capacity(total);
    for r in records {
        all.push(r.micros);
        let e = by_op.entry(r.op).or_insert_with(|| (0, 0, Vec::new()));
        e.0 += 1;
        e.1 += usize::from(!r.ok);
        e.2.push(r.micros);
    }
    all.sort_unstable();

    let secs = elapsed.as_secs_f64();
    println!();
    println!("================ quiz-bench report ================");
    println!("target      : {}", args.url);
    println!(
        "scenario    : {scenario}  concurrency={}  duration={:.1}s",
        args.concurrency, secs
    );
    println!(
        "requests    : {total} total  ({:.0} req/s)",
        total as f64 / secs
    );
    println!(
        "errors      : {errors} ({:.2}%)",
        100.0 * errors as f64 / total.max(1) as f64
    );
    println!();
    println!(
        "{:<14} {:>8} {:>8} {:>7} {:>10} {:>10} {:>10} {:>10}",
        "operation", "count", "req/s", "errors", "p50", "p95", "p99", "max"
    );
    println!("{}", "-".repeat(90));
    let mut ops: Vec<_> = by_op.into_iter().collect();
    ops.sort_by_key(|(_, (c, _, _))| std::cmp::Reverse(*c));
    for (op, (count, errs, mut lat)) in ops {
        lat.sort_unstable();
        println!(
            "{:<14} {:>8} {:>8.1} {:>7} {:>10} {:>10} {:>10} {:>10}",
            op,
            count,
            count as f64 / secs,
            errs,
            fmt_us(percentile(&lat, 0.50)),
            fmt_us(percentile(&lat, 0.95)),
            fmt_us(percentile(&lat, 0.99)),
            fmt_us(lat.last().copied().unwrap_or(0)),
        );
    }
    println!("{}", "-".repeat(90));
    println!(
        "{:<14} {:>8} {:>8.1} {:>7} {:>10} {:>10} {:>10} {:>10}",
        "ALL",
        total,
        total as f64 / secs,
        errors,
        fmt_us(percentile(&all, 0.50)),
        fmt_us(percentile(&all, 0.95)),
        fmt_us(percentile(&all, 0.99)),
        fmt_us(all.last().copied().unwrap_or(0)),
    );

    if errors > 0 {
        println!("\nNOTE: {errors} errors - check server logs (RUST_LOG) for details.");
    }
    Ok(())
}
