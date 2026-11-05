//! Attempt lifecycle + leaderboard repository.
//!
//! `submit` is the hot path: it loads, grades and persists an entire
//! attempt inside ONE BEGIN CONCURRENT transaction, so a conflict simply
//! re-runs the whole grading on retry.

use crate::error::{AppError, AppResult};
use crate::grading::{self, GradedAttempt};
use crate::ids::{new_id, now_ms};
use crate::model::{AttemptRow, LeaderboardEntry, QuizRow};
use sqlx::{Executor, Sqlite};

const ATTEMPT_COLS: &str =
    "id, quiz_id, user_id, status, started_at, submitted_at, score, max_score";

/// Start a new attempt for `user_id` on `quiz_id` (runs in a write tx).
pub async fn start(
    conn: &mut sqlx::SqliteConnection,
    quiz_id: i64,
    user_id: i64,
) -> AppResult<AttemptRow> {
    let quiz: Option<QuizRow> = sqlx::query_as("SELECT * FROM quizzes WHERE id = ?1")
        .bind(quiz_id)
        .fetch_optional(&mut *conn)
        .await?;
    let quiz = quiz.ok_or_else(|| AppError::NotFound("quiz".into()))?;
    if !quiz.is_public && quiz.creator_id != user_id {
        return Err(AppError::Forbidden);
    }

    if let Some(max) = quiz.max_attempts {
        let used: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE quiz_id = ?1 AND user_id = ?2")
                .bind(quiz_id)
                .bind(user_id)
                .fetch_one(&mut *conn)
                .await?;
        if used >= max {
            return Err(AppError::Conflict(
                "max attempts for this quiz reached".into(),
            ));
        }
    }

    let attempt = sqlx::query_as::<_, AttemptRow>(&format!(
        "INSERT INTO attempts (id, quiz_id, user_id, status, started_at)
         VALUES (?1, ?2, ?3, 'in_progress', ?4)
         RETURNING {ATTEMPT_COLS}"
    ))
    .bind(new_id())
    .bind(quiz_id)
    .bind(user_id)
    .bind(now_ms())
    .fetch_one(&mut *conn)
    .await?;
    Ok(attempt)
}

/// Outcome of a successful submission.
pub struct Submission {
    pub attempt: AttemptRow,
    pub graded: GradedAttempt,
}

/// Validate, grade and persist an attempt submission (runs in a write tx).
/// `answers` maps question_id -> selected option ids.
pub async fn submit(
    conn: &mut sqlx::SqliteConnection,
    attempt_id: i64,
    caller_id: i64,
    answers: &std::collections::HashMap<i64, Vec<i64>>,
) -> AppResult<Submission> {
    let attempt: AttemptRow = sqlx::query_as(&format!(
        "SELECT {ATTEMPT_COLS} FROM attempts WHERE id = ?1"
    ))
    .bind(attempt_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::NotFound("attempt".into()))?;

    if attempt.user_id != caller_id {
        return Err(AppError::Forbidden);
    }
    if attempt.status != "in_progress" {
        return Err(AppError::Conflict("attempt already submitted".into()));
    }

    // Time limit enforcement (2s grace for network latency).
    if let Some(limit) = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT q.time_limit_sec FROM quizzes q WHERE q.id = ?1",
    )
    .bind(attempt.quiz_id)
    .fetch_one(&mut *conn)
    .await?
    {
        let elapsed_ms = now_ms() - attempt.started_at;
        if elapsed_ms > limit * 1000 + 2000 {
            return Err(AppError::Conflict("time limit exceeded".into()));
        }
    }

    let questions = crate::repo::quizzes::quiz_questions(&mut *conn, attempt.quiz_id).await?;
    if questions.is_empty() {
        return Err(AppError::BadRequest("quiz has no questions".into()));
    }
    let options = crate::repo::quizzes::quiz_options(&mut *conn, attempt.quiz_id).await?;
    let questions = crate::repo::quizzes::assemble_questions(questions, options);

    let graded = grading::grade(&questions, answers);
    let submitted_at = now_ms();

    // Persist per-question answers in chunked multi-row inserts.
    // NOTE: plain `?` placeholders (NOT ?N) - each `?` is a distinct
    // parameter; repeated ?N would collapse all rows onto one value set.
    for chunk in graded.answers.chunks(64) {
        let mut sql = String::with_capacity(64 * 80);
        sql.push_str(
            "INSERT INTO answers (id, attempt_id, question_id, selected, is_correct, awarded, possible) VALUES ",
        );
        for (i, _) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str("(?, ?, ?, ?, ?, ?, ?)");
        }
        let mut q = sqlx::query(&sql);
        for a in chunk {
            q = q
                .bind(new_id())
                .bind(attempt_id)
                .bind(a.question_id)
                .bind(serde_json::to_string(&a.selected).expect("serialize ids"))
                .bind(a.correct)
                .bind(a.awarded)
                .bind(a.possible);
        }
        q.execute(&mut *conn).await?;
    }

    let updated = sqlx::query_as::<_, AttemptRow>(&format!(
        "UPDATE attempts SET status = 'graded', submitted_at = ?2, score = ?3, max_score = ?4
         WHERE id = ?1
         RETURNING {ATTEMPT_COLS}"
    ))
    .bind(attempt_id)
    .bind(submitted_at)
    .bind(graded.score)
    .bind(graded.max_score)
    .fetch_one(&mut *conn)
    .await?;

    metrics::counter!("attempts_submitted_total").increment(1);

    Ok(Submission {
        attempt: updated,
        graded,
    })
}

/// Top-N leaderboard for a quiz (graded attempts, best first).
pub async fn leaderboard<'e, E>(
    exec: E,
    quiz_id: i64,
    limit: i64,
) -> AppResult<Vec<LeaderboardEntry>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let entries = sqlx::query_as::<_, LeaderboardEntry>(
        "SELECT rank, user_id, username, score, max_score, submitted_at FROM (
             SELECT ROW_NUMBER() OVER (
                        ORDER BY a.score DESC, a.submitted_at ASC, a.id ASC) AS rank,
                    a.user_id, u.username, a.score, a.max_score, a.submitted_at
             FROM attempts a JOIN users u ON u.id = a.user_id
             WHERE a.quiz_id = ?1 AND a.status = 'graded' AND a.submitted_at IS NOT NULL
         ) LIMIT ?2",
    )
    .bind(quiz_id)
    .bind(limit)
    .fetch_all(exec)
    .await?;
    Ok(entries)
}

/// A user's attempts, optionally filtered by quiz, newest first.
pub async fn by_user<'e, E>(
    exec: E,
    user_id: i64,
    quiz_id: Option<i64>,
) -> AppResult<Vec<AttemptRow>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let rows = match quiz_id {
        Some(qid) => {
            sqlx::query_as::<_, AttemptRow>(&format!(
                "SELECT {ATTEMPT_COLS} FROM attempts WHERE user_id = ?1 AND quiz_id = ?2
             ORDER BY started_at DESC"
            ))
            .bind(user_id)
            .bind(qid)
            .fetch_all(exec)
            .await?
        }
        None => {
            sqlx::query_as::<_, AttemptRow>(&format!(
                "SELECT {ATTEMPT_COLS} FROM attempts WHERE user_id = ?1
             ORDER BY started_at DESC LIMIT 200"
            ))
            .bind(user_id)
            .fetch_all(exec)
            .await?
        }
    };
    Ok(rows)
}

/// Load one attempt plus its graded answers, owner-only.
pub async fn with_answers<'e, E>(
    exec: E,
    attempt_id: i64,
    caller_id: i64,
) -> AppResult<(AttemptRow, Vec<AnswerView>)>
where
    E: 'e + Send + Clone + Executor<'e, Database = Sqlite>,
{
    let attempt: AttemptRow = sqlx::query_as(&format!(
        "SELECT {ATTEMPT_COLS} FROM attempts WHERE id = ?1"
    ))
    .bind(attempt_id)
    .fetch_optional(exec.clone())
    .await?
    .ok_or_else(|| AppError::NotFound("attempt".into()))?;
    if attempt.user_id != caller_id {
        return Err(AppError::Forbidden);
    }
    let answers = sqlx::query_as::<_, AnswerView>(
        // Order by the question's position in the quiz (ids are random).
        "SELECT a.question_id, a.selected, a.is_correct, a.awarded, a.possible
         FROM answers a JOIN questions q ON q.id = a.question_id
         WHERE a.attempt_id = ?1
         ORDER BY q.position",
    )
    .bind(attempt_id)
    .fetch_all(exec)
    .await?;
    Ok((attempt, answers))
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AnswerView {
    pub question_id: i64,
    pub selected: String,
    pub is_correct: bool,
    pub awarded: f64,
    pub possible: f64,
}
