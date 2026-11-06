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
use sqlx::Connection;
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
/// The transaction runs in a task of its own and is **not cancelled with the
/// caller**: a handler that is dropped (the client went away, the request
/// timeout) while its write is in flight cannot leave a pooled connection
/// inside an open `BEGIN CONCURRENT` (sqlx's guard cannot cover the instant
/// between sending BEGIN and getting the guard back). The write simply
/// finishes, commits or rolls back, and the connection returns clean. That is
/// why `op` must own what it uses.
///
/// On success returns the operation's value. Metrics: transaction duration
/// histogram, commit and retry counters.
pub async fn write_tx<T, F>(state: &AppState, op: F) -> AppResult<T>
where
    T: Send + 'static,
    F: for<'c> FnMut(&'c mut SqliteConnection) -> TxFuture<'c, T> + Send + 'static,
{
    let state = state.clone();
    tokio::spawn(async move { run_write_tx(&state, op).await })
        .await
        .map_err(|e| AppError::Internal(format!("write transaction task: {e}")))?
}

async fn run_write_tx<T, F>(state: &AppState, mut op: F) -> AppResult<T>
where
    T: Send,
    F: FnMut(&mut SqliteConnection) -> TxFuture<'_, T>,
{
    let mode = state.cfg.tx_mode;
    let started = Instant::now();
    let mut attempt: u32 = 0;

    loop {
        // The connection lives only inside this block, so it is back in the
        // pool before a retry sleeps.
        let step = {
            let mut conn = state.pool.acquire().await.map_err(AppError::from)?;
            attempt_once(state, &mut conn, &mut op, started).await
        };
        match step {
            Step::Done(value) => return Ok(value),
            Step::Failed(err) => return Err(err),
            Step::Busy(err) => {
                if attempt < state.cfg.tx_max_retries {
                    metrics::counter!("tx_retries_total", "mode" => mode.as_str()).increment(1);
                    tracing::debug!(attempt, error = %err, "write transaction conflicted, retrying");
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt)).await;
                } else {
                    return Err(err);
                }
            }
        }
    }
}

/// How one attempt ended.
enum Step<T> {
    Done(T),
    /// Lock contention: worth another attempt.
    Busy(AppError),
    Failed(AppError),
}

async fn attempt_once<T, F>(
    state: &AppState,
    conn: &mut SqliteConnection,
    op: &mut F,
    started: Instant,
) -> Step<T>
where
    T: Send,
    F: FnMut(&mut SqliteConnection) -> TxFuture<'_, T>,
{
    let mode = state.cfg.tx_mode;
    let classify = |e: AppError| match &e {
        AppError::Database(db) if is_busy_error(db) => Step::Busy(e),
        _ => Step::Failed(e),
    };
    // The transaction is a sqlx guard: dropped at any await point (a client
    // that went away, the request timeout) it rolls back and the connection
    // is clean again.
    let mut txn = match conn.begin_with(mode.begin_stmt()).await {
        Ok(txn) => txn,
        // BEGIN itself can rarely hit SQLITE_BUSY (e.g. IMMEDIATE).
        Err(e) => return classify(e.into()),
    };
    match op(&mut txn).await {
        Err(err) => {
            // Roll back now (the guard would on drop) so the connection is
            // clean before it goes back.
            let _ = txn.rollback().await;
            classify(err)
        }
        Ok(value) => {
            // Serialize COMMITs in-process: cheaper than the SQLite
            // busy-handler spinning on the WAL WRITER lock.
            let committed = if state.cfg.commit_mutex {
                let _guard = state.commit_mutex.lock().await;
                txn.commit().await
            } else {
                txn.commit().await
            };
            match committed {
                Ok(()) => {
                    state
                        .commits
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    metrics::counter!("tx_commits_total", "mode" => mode.as_str()).increment(1);
                    metrics::histogram!("tx_duration_seconds", "mode" => mode.as_str())
                        .record(started.elapsed().as_secs_f64());
                    Step::Done(value)
                }
                // the guard dropped with the failed commit: rolled back
                Err(e) => classify(e.into()),
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
