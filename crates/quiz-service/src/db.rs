//! Database pool construction and migrations.
//!
//! Connection pragmas follow the begin-concurrent recommendations:
//! WAL journaling, NORMAL sync, generous per-connection cache and mmap,
//! in-memory temp store. All connections are read-write; BEGIN CONCURRENT
//! lets them proceed concurrently and the tx engine serializes COMMITs.

use crate::config::Config;
use crate::error::AppResult;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::path::PathBuf;
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

    // An older build must still start on a database a newer one has migrated (a rollback of
    // the image): it does not know the newer migrations, and that is not an error.
    let mut migrator = sqlx::migrate!();
    migrator.set_ignore_missing(true);
    migrator.run(&pool).await?;
    let applied: Vec<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations")
        .fetch_all(&pool)
        .await?;
    for version in applied {
        if !migrator.iter().any(|m| m.version == version) {
            tracing::warn!(
                version,
                "the database holds a migration this build does not know (a rolled-back image?)"
            );
        }
    }

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

/// Total size of the database files (the main file and its WAL) in bytes.
pub fn size_bytes(path: &std::path::Path) -> u64 {
    let len = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len());
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    len(path) + len(std::path::Path::new(&wal))
}

/// Export `quiz_db_size_bytes` now and every 30 s for the life of the process.
pub fn spawn_size_gauge(path: PathBuf) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tick.tick().await;
            metrics::gauge!("quiz_db_size_bytes").set(size_bytes(&path) as f64);
        }
    });
}
