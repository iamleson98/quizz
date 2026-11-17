//! What a game needs from persistence. A trait so the game can be tested
//! with paused time and no database (real I/O does not mix with a paused
//! clock).

use crate::error::AppResult;
use crate::model::QuestionWithOptions;
use crate::repo::games::{Batch, NewGame};
use crate::repo::running::{self, RunningRow};
use crate::repo::{games, quizzes};
use crate::state::AppState;
use crate::tx;
use futures::future::BoxFuture;
use std::sync::Arc;
use std::time::Duration;

/// How many times the last write of a game is tried again once its room has let go of it,
/// waiting 1 s, 2 s, 4 s … (at most a minute) in between: about a quarter of an hour.
const LATE_WRITE_TRIES: u32 = 20;
const LATE_WRITE_MAX_WAIT: Duration = Duration::from_secs(60);

/// Keeps trying the last write of a game that has ended: its room has moved on (it may host
/// the next game), so the results must not depend on the database being free at that
/// moment. A batch is written in one transaction, so trying it again never stores part of
/// it twice. The caller hands the task to the rooms, so that a shutdown waits for it.
pub fn write_later(store: Arc<dyn GameStore>, batch: Batch) -> tokio::task::JoinHandle<()> {
    metrics::counter!("quiz_late_writes_total").increment(1);
    tokio::spawn(async move {
        let mut wait = Duration::from_secs(1);
        for attempt in 1..=LATE_WRITE_TRIES {
            tokio::time::sleep(wait).await;
            match store.write(batch.clone()).await {
                Ok(()) => {
                    tracing::info!(
                        game = batch.game_id,
                        attempt,
                        "the results of an ended game were written"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, game = batch.game_id, attempt, "writing the results of an ended game failed again")
                }
            }
            wait = (wait * 2).min(LATE_WRITE_MAX_WAIT);
        }
        metrics::counter!("quiz_lost_writes_total").increment(1);
        tracing::error!(
            game = batch.game_id,
            "the results of an ended game could not be written: they are lost"
        );
    })
}

/// A quiz as a game plays it.
#[derive(Clone, Debug)]
pub struct LoadedQuiz {
    pub id: i64,
    pub title: String,
    pub questions: Arc<Vec<QuestionWithOptions>>,
}

pub trait GameStore: Send + Sync + 'static {
    /// The quiz of `owner_id`, with its questions.
    fn load_quiz(&self, quiz_id: i64, owner_id: String) -> BoxFuture<'_, AppResult<LoadedQuiz>>;
    /// Record a started game (and the use of its quiz).
    fn create_game(&self, game: NewGame) -> BoxFuture<'_, AppResult<()>>;
    /// Write players and answers (and the end of the game when asked).
    fn write(&self, batch: Batch) -> BoxFuture<'_, AppResult<()>>;
    /// Store the state of a room's running game, replacing the one before it.
    fn save_running(&self, row: RunningRow) -> BoxFuture<'_, AppResult<()>>;
    /// Forget a room's running game, if the row is still that game's (the room may host
    /// the next one already).
    fn delete_running(&self, room: String, game_id: i64) -> BoxFuture<'_, AppResult<()>>;
    /// Every stored running game that is still running (what a restart puts back); the rows
    /// of games that ended are deleted.
    fn load_running(&self) -> BoxFuture<'_, AppResult<Vec<RunningRow>>>;
}

/// The store over the service's SQLite.
pub struct DbStore {
    pub state: AppState,
}

impl GameStore for DbStore {
    fn load_quiz(&self, quiz_id: i64, owner_id: String) -> BoxFuture<'_, AppResult<LoadedQuiz>> {
        Box::pin(async move {
            let quiz = quizzes::owned(&self.state.pool, quiz_id, &owner_id).await?;
            let questions = quizzes::questions(&self.state.pool, quiz_id).await?;
            Ok(LoadedQuiz {
                id: quiz.id,
                title: quiz.title,
                questions: Arc::new(questions),
            })
        })
    }

    fn create_game(&self, game: NewGame) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            tx::write_tx(&self.state, move |conn| {
                let game = game.clone();
                Box::pin(async move { games::start(conn, &game).await })
            })
            .await
        })
    }

    fn write(&self, batch: Batch) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            tx::write_tx(&self.state, move |conn| {
                let batch = batch.clone();
                Box::pin(async move { games::write_batch(conn, &batch).await })
            })
            .await
        })
    }

    fn save_running(&self, row: RunningRow) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            tx::write_tx(&self.state, move |conn| {
                let row = row.clone();
                Box::pin(async move { running::save(conn, &row).await })
            })
            .await
        })
    }

    fn delete_running(&self, room: String, game_id: i64) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            tx::write_tx(&self.state, move |conn| {
                let room = room.clone();
                Box::pin(async move { running::delete(conn, &room, game_id).await })
            })
            .await
        })
    }

    fn load_running(&self) -> BoxFuture<'_, AppResult<Vec<RunningRow>>> {
        Box::pin(async move {
            tx::write_tx(&self.state, |conn| {
                Box::pin(async move { running::drop_ended(conn).await })
            })
            .await?;
            running::all(&self.state.pool).await
        })
    }
}
