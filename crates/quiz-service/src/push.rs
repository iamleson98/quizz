//! Pushing lesson-time spans to the Go server.
//!
//! The outbox table is the queue. The pusher sends what is pending in
//! batches of at most [`BATCH`], deletes what the server acknowledged
//! (answer 204), retries on network errors and 5xx with exponential backoff
//! and jitter, and drops what the server will never accept (404 for an
//! unknown session, any other 4xx). The call is idempotent by (game, user,
//! from) and a span's end only grows, so a repeat is harmless.

use crate::error::AppResult;
use crate::repo::spans::{self, Span};
use crate::state::AppState;
use crate::tx;
use serde_json::json;
use std::time::Duration;

/// Spans per request.
pub const BATCH: usize = 500;
/// First retry delay, and the longest.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(5 * 60);
/// Look at the outbox this often even when nobody woke the pusher.
const SWEEP_EVERY: Duration = Duration::from_secs(30);
const ROUTE: &str = "/api/v4/lms/internal/quiz/spans";

/// What the server said to a batch.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Accepted,
    /// Network error or 5xx: try again later.
    Retry,
    /// A 4xx: it will never accept this.
    Rejected,
}

pub struct Pusher {
    state: AppState,
    http: reqwest::Client,
    url: String,
}

impl Pusher {
    /// The pusher for the configured Go server, `None` when pushing is off.
    pub fn new(state: &AppState) -> Option<Self> {
        let base = state.cfg.lms_url.as_ref()?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .ok()?;
        Some(Pusher {
            state: state.clone(),
            http,
            url: format!("{base}{ROUTE}"),
        })
    }

    async fn send(&self, batch: &[Span]) -> Verdict {
        let body = json!({ "spans": batch.iter().map(|s| json!({
            "game_id": s.game_id.to_string(),
            "session_id": s.session_id,
            "user_id": s.user_id,
            "role": s.role,
            "from": s.from_ms,
            "to": s.to_ms,
        })).collect::<Vec<_>>() });
        let res = self
            .http
            .post(&self.url)
            .header(
                crate::server::INTERNAL_KEY_HEADER,
                self.state.cfg.internal_key.expose(),
            )
            .json(&body)
            .send()
            .await;
        match res {
            Ok(r) if r.status() == reqwest::StatusCode::NO_CONTENT => Verdict::Accepted,
            Ok(r) if r.status().is_client_error() => {
                tracing::warn!(status = %r.status(), spans = batch.len(), "the server refused lesson-time spans");
                Verdict::Rejected
            }
            Ok(r) => {
                tracing::warn!(status = %r.status(), "pushing lesson-time spans failed");
                Verdict::Retry
            }
            Err(e) => {
                tracing::warn!(error = %e, "pushing lesson-time spans failed");
                Verdict::Retry
            }
        }
    }

    async fn forget_batch(&self, batch: Vec<Span>, drop_them: bool) -> AppResult<()> {
        tx::write_tx(&self.state, move |conn| {
            let batch = batch.clone();
            Box::pin(async move {
                for s in &batch {
                    if drop_them {
                        spans::discard(conn, s).await?;
                    } else {
                        spans::acknowledge(conn, s).await?;
                    }
                }
                Ok(())
            })
        })
        .await
    }

    /// Send everything pending. With `retry` a failing server is waited for
    /// with backoff; without it the first failure ends the attempt.
    /// Returns true when the outbox has nothing pending.
    pub async fn drain(&self, retry: bool) -> bool {
        let mut delay = BACKOFF_FIRST;
        loop {
            let pending = match spans::pending(&self.state.pool, BATCH as i64).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = %e, "reading the spans outbox failed");
                    return false;
                }
            };
            if pending.is_empty() {
                self.report_depth().await;
                return true;
            }
            // Batches to send; a rejected batch is split to find the span
            // the server objects to, so one bad span does not sink the rest.
            let mut queue = vec![pending];
            while let Some(batch) = queue.pop() {
                match self.send(&batch).await {
                    Verdict::Accepted => {
                        metrics::counter!("quiz_spans_pushed_total").increment(batch.len() as u64);
                        if let Err(e) = self.forget_batch(batch, false).await {
                            tracing::error!(error = %e, "recording the acknowledged spans failed");
                            return false;
                        }
                        delay = BACKOFF_FIRST;
                    }
                    Verdict::Rejected if batch.len() > 1 => {
                        let (a, b) = batch.split_at(batch.len() / 2);
                        queue.push(b.to_vec());
                        queue.push(a.to_vec());
                    }
                    Verdict::Rejected => {
                        metrics::counter!("quiz_spans_dropped_total").increment(1);
                        if let Err(e) = self.forget_batch(batch, true).await {
                            tracing::error!(error = %e, "dropping a refused span failed");
                            return false;
                        }
                    }
                    Verdict::Retry => {
                        self.report_depth().await;
                        if !retry {
                            return false;
                        }
                        let jitter = rand::random::<f64>() * 0.5 + 0.75;
                        tokio::time::sleep(delay.mul_f64(jitter)).await;
                        delay = (delay * 2).min(BACKOFF_CAP);
                        break;
                    }
                }
            }
        }
    }

    async fn report_depth(&self) {
        if let Ok(n) = spans::depth(&self.state.pool).await {
            metrics::gauge!("quiz_spans_outbox_depth").set(n as f64);
        }
    }

    /// Push for the life of the process: when woken, and now and then.
    pub async fn run(self) {
        loop {
            self.drain(true).await;
            tokio::select! {
                _ = self.state.spans_ready.notified() => {}
                _ = tokio::time::sleep(SWEEP_EVERY) => {}
            }
        }
    }

    /// One last attempt, as the service stops.
    pub async fn flush_once(&self) {
        self.drain(false).await;
    }
}
