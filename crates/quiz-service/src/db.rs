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
