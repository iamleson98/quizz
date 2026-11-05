//! Database pool construction, migrations and demo seeding.
//!
//! Connection pragmas follow the begin-concurrent recommendations:
//! WAL journaling, NORMAL sync, generous per-connection cache and mmap,
//! in-memory temp store. All connections are read-write; BEGIN CONCURRENT
//! lets them proceed concurrently and the tx engine serializes COMMITs.

use crate::config::Config;
use crate::error::AppResult;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::time::Duration;

/// Build connect options with the performance pragma set.
pub fn connect_options(cfg: &Config) -> SqliteConnectOptions {
    let mut opts = SqliteConnectOptions::new()
        .filename(&cfg.database_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(cfg.busy_timeout_ms))
        // BEGIN CONCURRENT requires WAL; a big page cache avoids mid-txn
        // pressure (concurrent txns cannot spill their cache).
        .pragma("cache_size", format!("-{}", cfg.cache_size_kib))
        .pragma("mmap_size", cfg.mmap_size.to_string())
        .pragma("temp_store", "MEMORY")
        .pragma("wal_autocheckpoint", "1000");

    // `unix-excl` uses more efficient locking for single-process deployments
    // (recommended by the begin-concurrent docs). `unix` keeps the file
    // shareable with external tools such as the sqlite3 CLI.
    if cfg.vfs != "unix" {
        opts = opts.vfs(cfg.vfs.clone());
    }
    opts
}

/// Create the pool, run migrations, and return the pool plus the runtime
/// SQLite version string (useful to prove we are running the custom build).
pub async fn connect_pool(cfg: &Config) -> AppResult<(SqlitePool, String)> {
    if let Some(parent) = cfg.database_path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    let opts = connect_options(cfg);
    let pool = SqlitePoolOptions::new()
        .max_connections(cfg.pool_max)
        .min_connections(cfg.pool_min)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await?;

    sqlx::migrate!().run(&pool).await?;

    let (version,): (String,) = sqlx::query_as("SELECT sqlite_version()")
        .fetch_one(&pool)
        .await?;

    Ok((pool, version))
}

/// Readiness probe query.
pub async fn ping(pool: &SqlitePool) -> AppResult<()> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

/// Idempotent demo data: `demo` / `demo1234` user + a mixed demo quiz.
/// Returns the demo quiz id when seeding created it (None if it existed).
pub async fn seed_demo(pool: &SqlitePool) -> AppResult<()> {
    use crate::AppState;
    use crate::ids::{new_id, now_ms};
    use crate::tx;

    // A tiny state purely for the tx engine primitives.
    let cfg = Config {
        listen_addr: String::new(),
        database_path: std::path::PathBuf::from("seed"),
        jwt_secret: String::new(),
        jwt_ttl_hours: 1,
        pool_max: 4,
        pool_min: 1,
        busy_timeout_ms: 5000,
        tx_mode: crate::config::TxMode::Concurrent,
        tx_max_retries: 64,
        commit_mutex: true,
        vfs: "unix".into(),
        cache_size_kib: 8192,
        mmap_size: 0,
        otel_endpoint: None,
        log_format: "pretty".into(),
        seed_demo: false,
    };
    let state = AppState::for_pool(pool.clone(), cfg);

    let hash = crate::auth::hash_password("demo1234")?;
    tx::write_tx(&state, move |conn| {
        let hash = hash.clone();
        Box::pin(async move {
            let demo_id: Option<i64> =
                sqlx::query_scalar("SELECT id FROM users WHERE username = 'demo'")
                    .fetch_optional(&mut *conn)
                    .await?;
            let demo_id = match demo_id {
                Some(id) => id,
                None => {
                    let id = new_id();
                    sqlx::query("INSERT INTO users (id, username, password_hash, created_at) VALUES (?1, 'demo', ?2, ?3)")
                        .bind(id)
                        .bind(&hash)
                        .bind(now_ms())
                        .execute(&mut *conn)
                        .await?;
                    id
                }
            };

            let existing: Option<i64> =
                sqlx::query_scalar("SELECT id FROM quizzes WHERE creator_id = ?1 AND title = 'General Knowledge Demo'")
                    .bind(demo_id)
                    .fetch_optional(&mut *conn)
                    .await?;
            if existing.is_some() {
                return Ok(());
            }

            let quiz_id = new_id();
            let now = now_ms();
            sqlx::query(
                "INSERT INTO quizzes (id, creator_id, title, description, is_public, time_limit_sec, max_attempts, created_at, updated_at)
                 VALUES (?1, ?2, 'General Knowledge Demo', 'Three questions: single choice, multi select and true/false.', 1, NULL, NULL, ?3, ?3)",
            )
            .bind(quiz_id)
            .bind(demo_id)
            .bind(now)
            .execute(&mut *conn)
            .await?;

            // (kind, text, points, options: &[(text, is_correct)])
            type DemoQ<'a> = (&'a str, &'a str, f64, &'a [(&'a str, bool)]);
            let questions: &[DemoQ] = &[
                ("single", "Which planet is known as the Red Planet?", 1.0,
                 &[("Venus", false), ("Mars", true), ("Jupiter", false)]),
                ("multi", "Select all prime numbers.", 2.0,
                 &[("2", true), ("9", false), ("13", true), ("15", false)]),
                ("boolean", "The Pacific Ocean is the largest ocean on Earth.", 1.0,
                 &[("True", true), ("False", false)]),
            ];

            for (pos, (kind, text, points, options)) in questions.iter().enumerate() {
                let qid = new_id();
                sqlx::query(
                    "INSERT INTO questions (id, quiz_id, position, kind, text, points, explanation)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                )
                .bind(qid)
                .bind(quiz_id)
                .bind(pos as i64 + 1)
                .bind(kind)
                .bind(text)
                .bind(points)
                .execute(&mut *conn)
                .await?;
                for (opos, (otext, correct)) in options.iter().enumerate() {
                    sqlx::query(
                        "INSERT INTO options (id, question_id, position, text, is_correct)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )
                    .bind(new_id())
                    .bind(qid)
                    .bind(opos as i64 + 1)
                    .bind(otext)
                    .bind(*correct)
                    .execute(&mut *conn)
                    .await?;
                }
            }
            Ok(())
        })
    })
    .await?;
    Ok(())
}
