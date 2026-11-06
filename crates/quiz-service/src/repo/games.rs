//! Games, players and answers: what the engine writes and what the results
//! screen reads.

use crate::error::{AppError, AppResult};
use crate::retention::{KEPT_QUIZ_TTL_MS, RESULTS_TTL_MS, UNKEPT_QUIZ_TTL_MS};
use sqlx::{Executor, Sqlite, SqliteConnection};
use std::collections::HashMap;

/// A game as it starts.
#[derive(Clone, Debug)]
pub struct NewGame {
    pub id: i64,
    pub quiz_id: i64,
    pub session_id: String,
    pub class_id: String,
    pub host_id: String,
    pub started_at: i64,
    /// 0 for a race, 1 for a quest.
    pub mode: i64,
}

/// One player's totals, upserted by id.
#[derive(Clone, Debug)]
pub struct PlayerWrite {
    pub id: i64,
    /// `None` once the person has been forgotten.
    pub user_id: Option<String>,
    pub name: String,
    pub joined_at: i64,
    pub score: i64,
    pub total_ms: i64,
    pub correct_count: i64,
    pub answered: i64,
    pub best_streak: i64,
}

/// One answer, written when its question closes.
#[derive(Clone, Debug)]
pub struct AnswerWrite {
    pub id: i64,
    pub player_id: i64,
    pub question_id: i64,
    pub selected: Vec<i64>,
    pub correct: bool,
    pub credit: f64,
    /// Points earned, the streak bonus included.
    pub points: i64,
    /// The streak bonus part of `points`.
    pub bonus: i64,
    pub time_ms: i64,
}

/// Everything one transaction writes for a game.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub game_id: i64,
    pub players: Vec<PlayerWrite>,
    pub answers: Vec<AnswerWrite>,
    /// Set on the last write of a game.
    pub ended_at: Option<i64>,
}

/// Record a started game and count the start as a use of its quiz.
pub async fn start(conn: &mut SqliteConnection, game: &NewGame) -> AppResult<()> {
    let res = sqlx::query(
        "UPDATE quizzes SET expires_at = ?2 + CASE kept WHEN 1 THEN ?3 ELSE ?4 END WHERE id = ?1",
    )
    .bind(game.quiz_id)
    .bind(game.started_at)
    .bind(KEPT_QUIZ_TTL_MS)
    .bind(UNKEPT_QUIZ_TTL_MS)
    .execute(&mut *conn)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("quiz".into()));
    }
    sqlx::query(
        "INSERT INTO games (id, quiz_id, session_id, class_id, host_id, started_at, mode)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(game.id)
    .bind(game.quiz_id)
    .bind(&game.session_id)
    .bind(&game.class_id)
    .bind(&game.host_id)
    .bind(game.started_at)
    .bind(game.mode)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Write a batch of players and answers, and close the game when asked.
pub async fn write_batch(conn: &mut SqliteConnection, batch: &Batch) -> AppResult<()> {
    for p in &batch.players {
        sqlx::query(
            "INSERT INTO players (id, game_id, user_id, name, joined_at, score, total_ms, correct_count, answered, best_streak)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                user_id = excluded.user_id, name = excluded.name, score = excluded.score,
                total_ms = excluded.total_ms, correct_count = excluded.correct_count,
                answered = excluded.answered, best_streak = excluded.best_streak",
        )
        .bind(p.id)
        .bind(batch.game_id)
        .bind(&p.user_id)
        .bind(&p.name)
        .bind(p.joined_at)
        .bind(p.score)
        .bind(p.total_ms)
        .bind(p.correct_count)
        .bind(p.answered)
        .bind(p.best_streak)
        .execute(&mut *conn)
        .await?;
    }
    for a in &batch.answers {
        sqlx::query(
            "INSERT INTO answers (id, game_id, player_id, question_id, selected, correct, credit, points, bonus, time_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(a.id)
        .bind(batch.game_id)
        .bind(a.player_id)
        .bind(a.question_id)
        .bind(serde_json::to_string(&a.selected).unwrap_or_else(|_| "[]".into()))
        .bind(a.correct)
        .bind(a.credit)
        .bind(a.points)
        .bind(a.bonus)
        .bind(a.time_ms)
        .execute(&mut *conn)
        .await?;
    }
    if let Some(ended_at) = batch.ended_at {
        sqlx::query("UPDATE games SET ended_at = ?2, expires_at = ?3 WHERE id = ?1")
            .bind(batch.game_id)
            .bind(ended_at)
            .bind(ended_at + RESULTS_TTL_MS)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct PlayerResult {
    pub rank: i64,
    pub name: String,
    pub anonymous: bool,
    pub score: i64,
    pub total_ms: i64,
    pub correct_count: i64,
    pub answered_count: i64,
    pub best_streak: i64,
}

#[derive(Clone, Debug)]
pub struct QuestionResult {
    pub question_id: i64,
    pub position: i64,
    pub text: String,
    pub answered_count: i64,
    pub correct_count: i64,
}

#[derive(Clone, Debug)]
pub struct GameResult {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub expires_at: i64,
    /// 0 race, 1 quest.
    pub mode: i64,
    pub players: Vec<PlayerResult>,
    pub questions: Vec<QuestionResult>,
}

/// `game_id, user_id, name, score, total_ms, correct_count, answered, best_streak`
type PlayerRow = (i64, Option<String>, String, i64, i64, i64, i64, i64);

/// Most recent ended games of a quiz returned by [`results`].
const MAX_RESULTS: i64 = 100;

/// Scoreboards of a quiz's ended games, newest first.
pub async fn results<'e, E>(exec: E, quiz_id: i64) -> AppResult<Vec<GameResult>>
where
    E: 'e + Send + Clone + Executor<'e, Database = Sqlite>,
{
    let games: Vec<(i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT id, started_at, ended_at, expires_at, mode FROM games
         WHERE quiz_id = ?1 AND ended_at IS NOT NULL
         ORDER BY started_at DESC, id DESC LIMIT ?2",
    )
    .bind(quiz_id)
    .bind(MAX_RESULTS)
    .fetch_all(exec.clone())
    .await?;
    if games.is_empty() {
        return Ok(Vec::new());
    }

    let players: Vec<PlayerRow> = sqlx::query_as(
        "SELECT p.game_id, p.user_id, p.name, p.score, p.total_ms, p.correct_count,
                p.answered, p.best_streak
         FROM players p JOIN games g ON g.id = p.game_id
         WHERE g.quiz_id = ?1
         ORDER BY p.game_id, p.score DESC, p.total_ms ASC, p.joined_at ASC, p.id",
    )
    .bind(quiz_id)
    .fetch_all(exec.clone())
    .await?;
    let mut players_by_game: HashMap<i64, Vec<PlayerResult>> = HashMap::new();
    for (game_id, user_id, name, score, total_ms, correct_count, answered_count, best_streak) in
        players
    {
        let list = players_by_game.entry(game_id).or_default();
        let rank = list.len() as i64 + 1;
        list.push(PlayerResult {
            rank,
            name,
            anonymous: user_id.is_none(),
            score,
            total_ms,
            correct_count,
            answered_count,
            best_streak,
        });
    }

    let stats: Vec<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT a.game_id, a.question_id, COUNT(*), COALESCE(SUM(a.correct), 0)
         FROM answers a JOIN games g ON g.id = a.game_id
         WHERE g.quiz_id = ?1 GROUP BY a.game_id, a.question_id",
    )
    .bind(quiz_id)
    .fetch_all(exec.clone())
    .await?;
    let stats: HashMap<(i64, i64), (i64, i64)> = stats
        .into_iter()
        .map(|(g, q, answered, correct)| ((g, q), (answered, correct)))
        .collect();
    let questions: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT id, position, text FROM questions WHERE quiz_id = ?1 ORDER BY position, id",
    )
    .bind(quiz_id)
    .fetch_all(exec)
    .await?;

    Ok(games
        .into_iter()
        .map(|(id, started_at, ended_at, expires_at, mode)| GameResult {
            id,
            started_at,
            ended_at,
            expires_at,
            mode,
            players: players_by_game.remove(&id).unwrap_or_default(),
            // a quest keeps no per-question results: nobody shares a question
            questions: questions
                .iter()
                .filter(|_| mode == 0)
                .map(|(qid, position, text)| {
                    let (answered_count, correct_count) =
                        stats.get(&(id, *qid)).copied().unwrap_or((0, 0));
                    QuestionResult {
                        question_id: *qid,
                        position: *position,
                        text: text.clone(),
                        answered_count,
                        correct_count,
                    }
                })
                .collect(),
        })
        .collect())
}

/// What `forget` removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Forgotten {
    pub quizzes: u64,
    pub players: u64,
}

/// Delete everything of one person: their quizzes (with the games on them)
/// and their name and id on player rows in other teachers' games.
pub async fn forget(conn: &mut SqliteConnection, user_id: &str) -> AppResult<Forgotten> {
    let quizzes = sqlx::query("DELETE FROM quizzes WHERE owner_id = ?1")
        .bind(user_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    let players = sqlx::query("UPDATE players SET user_id = NULL, name = '' WHERE user_id = ?1")
        .bind(user_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(Forgotten { quizzes, players })
}
