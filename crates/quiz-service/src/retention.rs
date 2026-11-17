//! Nothing lasts for ever.
//!
//! Every quiz carries the moment it expires and every ended game carries the
//! moment its results expire, and the stored state of a running game (what a
//! restart puts back) lasts only as long as the game; a sweep deletes what has
//! passed. The four periods live here and nowhere else.
//!
//! *Use* of a quiz is saving an edit, starting a game on it, or keeping /
//! un-keeping it; looking at it is not a use. Each use moves its expiry to
//! `now + period`, where the period depends on whether the quiz is kept.

use crate::error::AppResult;
use crate::state::AppState;
use crate::tx;
use std::time::Duration;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A quiz that is not kept goes this long after its last use.
pub const UNKEPT_QUIZ_TTL_MS: i64 = 7 * DAY_MS;
/// A kept quiz goes this long after its last use.
pub const KEPT_QUIZ_TTL_MS: i64 = 90 * DAY_MS;
/// A game's results (players and answers) go this long after it ended.
pub const RESULTS_TTL_MS: i64 = 90 * DAY_MS;
/// The stored state of a running game (what a restart puts back) goes this long after it
/// was last written. A running game writes it at least at every phase and no game lasts a
/// day, so a row older than this belongs to no game.
pub const RUNNING_GAME_TTL_MS: i64 = DAY_MS;

/// How often the sweep runs after the one at startup.
pub const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The moment a quiz used at `now_ms` expires.
pub fn quiz_expiry(kept: bool, now_ms: i64) -> i64 {
    now_ms
        + if kept {
            KEPT_QUIZ_TTL_MS
        } else {
            UNKEPT_QUIZ_TTL_MS
        }
}

/// What a sweep removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub quizzes: u64,
    pub games: u64,
    /// Stored running games that were not written for [`RUNNING_GAME_TTL_MS`].
    pub running_games: u64,
}

/// Delete what has expired as of `now_ms`: quizzes (with their games), then
/// games whose results have outlived their period while the quiz stays, and
/// the stored state of running games that were not written for a day (the
/// game is closed in the results). Quizzes with a running game are left alone.
pub async fn sweep(state: &AppState, now_ms: i64) -> AppResult<Swept> {
    let running = state.rooms.running_quiz_ids();
    tx::write_tx(state, move |conn| {
        let running = running.clone();
        Box::pin(async move {
            let mut quizzes = 0;
            let expired: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM quizzes WHERE expires_at <= ?1")
                    .bind(now_ms)
                    .fetch_all(&mut *conn)
                    .await?;
            for id in expired.into_iter().filter(|id| !running.contains(id)) {
                sqlx::query("DELETE FROM quizzes WHERE id = ?1")
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
                quizzes += 1;
            }
            let games =
                sqlx::query("DELETE FROM games WHERE expires_at IS NOT NULL AND expires_at <= ?1")
                    .bind(now_ms)
                    .execute(&mut *conn)
                    .await?
                    .rows_affected();
            let stale = now_ms - RUNNING_GAME_TTL_MS;
            sqlx::query(
                "UPDATE games SET ended_at = ?1, expires_at = ?1 + ?2
                 WHERE ended_at IS NULL
                   AND id IN (SELECT game_id FROM running_games WHERE updated_at <= ?3)",
            )
            .bind(now_ms)
            .bind(RESULTS_TTL_MS)
            .bind(stale)
            .execute(&mut *conn)
            .await?;
            let running_games = sqlx::query("DELETE FROM running_games WHERE updated_at <= ?1")
                .bind(stale)
                .execute(&mut *conn)
                .await?
                .rows_affected();
            Ok(Swept {
                quizzes,
                games,
                running_games,
            })
        })
    })
    .await
}

/// A game that was running when the service stopped and was not put back
/// never ended properly: close it so its results start their own countdown.
/// At startup, once the stored running games are back in their rooms, every
/// unended game that has no stored state is an orphan.
pub async fn close_orphaned_games(state: &AppState, now_ms: i64) -> AppResult<u64> {
    tx::write_tx(state, move |conn| {
        Box::pin(async move {
            let res = sqlx::query(
                "UPDATE games SET ended_at = ?1, expires_at = ?1 + ?2
                 WHERE ended_at IS NULL AND id NOT IN (SELECT game_id FROM running_games)",
            )
            .bind(now_ms)
            .bind(RESULTS_TTL_MS)
            .execute(&mut *conn)
            .await?;
            Ok(res.rows_affected())
        })
    })
    .await
}

/// Run the startup sweep now, then once a day, for the life of the process.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        match close_orphaned_games(&state, state.now_ms()).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(games = n, "closed games left running by the last process"),
            Err(e) => tracing::error!(error = %e, "closing orphaned games failed"),
        }
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tick.tick().await;
            match sweep(&state, state.now_ms()).await {
                Ok(s) => tracing::info!(
                    quizzes = s.quizzes,
                    games = s.games,
                    running_games = s.running_games,
                    "expiry sweep done"
                ),
                Err(e) => tracing::error!(error = %e, "expiry sweep failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_depends_on_kept() {
        assert_eq!(quiz_expiry(false, 1000), 1000 + 7 * DAY_MS);
        assert_eq!(quiz_expiry(true, 1000), 1000 + 90 * DAY_MS);
    }
}
