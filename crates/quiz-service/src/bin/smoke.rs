//! Smoke test: prove that sqlx links against OUR custom SQLite build
//! (begin-concurrent branch, 3.55.0) and that BEGIN CONCURRENT works.

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Executor};

#[tokio::main]
async fn main() -> sqlx::Result<()> {
    let path = std::env::temp_dir().join("quiz-smoke.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));

    let mut conn = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await?;

    let (version,): (String,) = sqlx::query_as("SELECT sqlite_version()")
        .fetch_one(&mut conn)
        .await?;
    println!("sqlite_version() = {version}");
    assert!(
        version.starts_with("3.55.0"),
        "expected our custom 3.55.0 build, got {version}"
    );

    conn.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
        .await?;

    // The statement only parses on a begin-concurrent patched SQLite.
    conn.execute("BEGIN CONCURRENT").await?;
    sqlx::query("INSERT INTO t(a, b) VALUES (1, 'begin-concurrent works')")
        .execute(&mut conn)
        .await?;
    conn.execute("COMMIT").await?;

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM t")
        .fetch_one(&mut conn)
        .await?;
    assert_eq!(n, 1);

    // journal_mode must be wal for BEGIN CONCURRENT
    let (mode,): (String,) = sqlx::query_as("PRAGMA journal_mode")
        .fetch_one(&mut conn)
        .await?;
    println!("journal_mode = {mode}");

    println!("SMOKE OK: sqlx <-> custom begin-concurrent SQLite linkage verified");
    Ok(())
}
