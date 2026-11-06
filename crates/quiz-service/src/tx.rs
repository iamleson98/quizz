//! The BEGIN CONCURRENT write-transaction engine.
//!
//! Every write in the service flows through [`write_tx`]:
//!
//! 1. acquire a pooled connection,
//! 2. `BEGIN CONCURRENT` (or DEFERRED/IMMEDIATE, per config, for baselining),
//! 3. run the operation against the connection,
//! 4. COMMIT under an in-process mutex - the begin-concurrent docs
//!    recommend serializing COMMITs with an application mutex instead of
//!    letting the SQLite busy-handler spin when all writers share one
//!    process,
//! 5. on SQLITE_BUSY / SQLITE_BUSY_SNAPSHOT: ROLLBACK, back off with jitter,
//!    restart the whole operation on a fresh attempt.
//!
//! Retry semantics require the operation closure to be idempotent and
//! side-effect free until commit; all repo functions passed here only touch
//! the database inside the transaction.

use crate::error::{AppError, AppResult, is_busy_error};
use crate::state::AppState;
use sqlx::Executor;
use sqlx::sqlite::SqliteConnection;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

/// A boxed future returned by write operations; borrows the connection for
/// the duration of the transaction body.
pub type TxFuture<'a, T> = Pin<Box<dyn Future<Output = AppResult<T>> + Send + 'a>>;

/// Backoff before retry attempt `attempt` (0-based): exponential with jitter.
fn backoff(attempt: u32) -> Duration {
    let base_ms = 1u64 << attempt.min(6); // 1, 2, 4, ... 64ms
    let cap_ms = 50u64;
    let jitter = rand::random::<f64>() * base_ms as f64;
    Duration::from_micros(((jitter + base_ms as f64) * 1000.0).min(cap_ms as f64 * 1000.0) as u64)
}

/// Run `op` inside a write transaction, retrying the whole transaction on
/// lock contention (SQLITE_BUSY*), up to `config.tx_max_retries` retries.
///
/// On success returns the operation's value. Metrics: transaction duration
/// histogram, commit and retry counters.
pub async fn write_tx<T, F>(state: &AppState, mut op: F) -> AppResult<T>
where
    T: Send,
    F: FnMut(&mut SqliteConnection) -> TxFuture<'_, T>,
{
    let mode = state.cfg.tx_mode;
    let started = Instant::now();
    let mut attempt: u32 = 0;

    loop {
        let mut conn = state.pool.acquire().await.map_err(AppError::from)?;

        if let Err(e) = conn.execute(state.cfg.tx_mode.begin_stmt()).await {
            // BEGIN itself can rarely hit SQLITE_BUSY (e.g. IMMEDIATE).
            if is_busy_error(&e) && attempt < state.cfg.tx_max_retries {
                metrics::counter!("tx_retries_total", "mode" => mode.as_str()).increment(1);
                attempt += 1;
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
            return Err(e.into());
        }

        match op(&mut conn).await {
            Err(err) => {
                // Best-effort rollback; the connection returns to the pool
                // either way, sqlx also rolls back on drop if needed.
                let _ = conn.execute("ROLLBACK").await;
                // Operations in a CONCURRENT txn rarely surface busy errors
                // themselves, but handle it for the IMMEDIATE/DEFERRED modes.
                if matches!(err, AppError::Database(ref e) if is_busy_error(e))
                    && attempt < state.cfg.tx_max_retries
                {
                    metrics::counter!("tx_retries_total", "mode" => mode.as_str()).increment(1);
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                return Err(err);
            }
            Ok(value) => {
                // Serialize COMMITs in-process: cheaper than the SQLite
                // busy-handler spinning on the WAL WRITER lock.
                let commit = async {
                    if state.cfg.commit_mutex {
                        let _guard = state.commit_mutex.lock().await;
                        conn.execute("COMMIT").await
                    } else {
                        conn.execute("COMMIT").await
                    }
                };

                match commit.await {
                    Ok(_) => {
                        state
                            .commits
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        metrics::counter!("tx_commits_total", "mode" => mode.as_str()).increment(1);
                        metrics::histogram!("tx_duration_seconds", "mode" => mode.as_str())
                            .record(started.elapsed().as_secs_f64());
                        return Ok(value);
                    }
                    Err(e) => {
                        let _ = conn.execute("ROLLBACK").await;
                        if is_busy_error(&e) && attempt < state.cfg.tx_max_retries {
                            metrics::counter!("tx_retries_total", "mode" => mode.as_str())
                                .increment(1);
                            tracing::debug!(
                                attempt,
                                error = %e,
                                "write transaction conflicted, retrying"
                            );
                            attempt += 1;
                            tokio::time::sleep(backoff(attempt)).await;
                            continue;
                        }
                        return Err(e.into());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_stays_bounded() {
        for a in 0..40 {
            let d = backoff(a);
            assert!(d.as_millis() <= 51, "backoff({a}) = {d:?}");
        }
    }
}
