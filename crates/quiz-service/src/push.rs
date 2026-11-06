//! Pushing lesson-time spans to the Go server.
//!
//! The outbox table is the queue. The pusher sends what is pending in
//! batches of at most [`BATCH`], deletes what the server acknowledged
//! (answer 204), retries on network errors and 5xx with exponential backoff
//! and jitter, and drops only what the server will certainly never accept (a
//! malformed batch: 400, 413, 422; a span of a session it says it does not
//! know: a 404 with the id `session_unknown`). Everything else, 401, 403, 429 or
//! a 404 without that id included, is waited out: the outbox keeps the spans. The call is idempotent by (game, user,
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

/// The error id the Go server answers 404 with when the session of a span is
/// unknown to it.
pub const SESSION_UNKNOWN: &str = "app.lms.quiz.spans.session_unknown.app_error";

/// What the server said to a batch.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Accepted,
    /// Anything that may pass: a network error, a 5xx, 401 or 403 (the feature
    /// is off or the keys differ right now), 408, 429 (rate limited), a 404
    /// from a server that does not have the route yet. The outbox is
    /// untouched and the push is tried again with backoff.
    Retry,
    /// What the server will never accept: a malformed or too large batch
    /// (400, 413, 422) or a span of a session it does not know (a 404 that
    /// says so).
    Rejected,
}

/// Decide from the status and body of a non-204 answer. Lesson time feeds the
/// fee and attendance, so a span is dropped only when it is certain that the
/// server will never take it.
fn classify(status: u16, body: &str) -> Verdict {
    match status {
        400 | 413 | 422 => Verdict::Rejected,
        404 => {
            let id = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.get("id").and_then(|i| i.as_str().map(String::from)));
            if id.as_deref() == Some(SESSION_UNKNOWN) {
                Verdict::Rejected
            } else {
                Verdict::Retry
            }
        }
        _ => Verdict::Retry,
    }
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
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                let verdict = classify(status.as_u16(), &body);
                match verdict {
                    Verdict::Rejected => {
                        tracing::warn!(%status, spans = batch.len(), "the server refused lesson-time spans for good")
                    }
                    _ => {
                        tracing::warn!(%status, "the server did not take the lesson-time spans: will retry")
                    }
                }
                verdict
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
    pub async fn run(self: std::sync::Arc<Self>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_certain_failures_drop_spans() {
        for s in [400, 413, 422] {
            assert_eq!(classify(s, ""), Verdict::Rejected, "{s}");
        }
        let unknown = format!(r#"{{"id":"{SESSION_UNKNOWN}","message":"x"}}"#);
        assert_eq!(classify(404, &unknown), Verdict::Rejected);
        for (status, body) in [
            (404, ""),
            (404, "not json"),
            (404, r#"{"id":"api.context.404.app_error"}"#),
            (401, ""),
            (403, ""),
            (408, ""),
            (429, ""),
            (500, ""),
            (502, ""),
            (503, ""),
            (302, ""),
        ] {
            assert_eq!(classify(status, body), Verdict::Retry, "{status} {body}");
        }
    }
}
