//! What a game needs from persistence. A trait so the game can be tested
//! with paused time and no database (real I/O does not mix with a paused
//! clock).

use crate::error::AppResult;
use crate::model::QuestionWithOptions;
use crate::repo::games::{Batch, NewGame};
use crate::repo::spans::Span;
use crate::repo::{games, quizzes, spans};
use crate::state::AppState;
use crate::tx;
use futures::future::BoxFuture;
use std::sync::Arc;
use tokio::sync::Notify;

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
    /// Put spans in the outbox.
    fn write_spans(&self, spans: Vec<Span>) -> BoxFuture<'_, AppResult<()>>;
}

/// The store over the service's SQLite.
pub struct DbStore {
    pub state: AppState,
    /// Woken when the outbox has something new for the pusher.
    pub spans_ready: Arc<Notify>,
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

    fn write_spans(&self, list: Vec<Span>) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            tx::write_tx(&self.state, move |conn| {
                let list = list.clone();
                Box::pin(async move {
                    for s in &list {
                        spans::upsert(conn, s).await?;
                    }
                    Ok(())
                })
            })
            .await?;
            self.spans_ready.notify_one();
            Ok(())
        })
    }
}
