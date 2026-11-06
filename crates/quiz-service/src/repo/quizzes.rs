//! Quiz bank: quizzes, questions and options.
//!
//! A quiz is always saved whole (title plus the full question list). A saved
//! question or option that carries an id keeps its identity, so the results
//! of games already played on the quiz stay attached to it.

use crate::error::{AppError, AppResult, FieldError};
use crate::ids::new_id;
use crate::model::{OptionRow, QuestionKind, QuestionRow, QuestionWithOptions, QuizRow};
use crate::retention::quiz_expiry;
use sqlx::{Executor, Sqlite, SqliteConnection};
use std::collections::{HashMap, HashSet};

/// Limits of the bank, enforced on every save.
pub mod limits {
    pub const MAX_QUIZZES_PER_OWNER: i64 = 200;
    pub const MAX_QUESTIONS: usize = 100;
    pub const MAX_TITLE_CHARS: usize = 140;
    pub const MAX_DESCRIPTION_CHARS: usize = 500;
    pub const MAX_QUESTION_CHARS: usize = 500;
    pub const MAX_OPTION_CHARS: usize = 200;
    pub const MAX_EXPLANATION_CHARS: usize = 500;
    pub const MIN_OPTIONS: usize = 2;
    pub const MAX_OPTIONS: usize = 6;
    pub const MIN_TIME_LIMIT_SEC: i64 = 5;
    pub const MAX_TIME_LIMIT_SEC: i64 = 120;
    pub const DEFAULT_TIME_LIMIT_SEC: i64 = 20;
    pub const MAX_PLAYERS_PER_GAME: usize = 500;
}

const QUIZ_COLS: &str =
    "id, owner_id, title, description, kept, expires_at, created_at, updated_at";

pub struct NewQuiz {
    pub title: String,
    pub description: String,
    pub questions: Vec<NewQuestion>,
}

pub struct NewQuestion {
    /// Present when the question already exists in the quiz being saved.
    pub id: Option<i64>,
    pub kind: QuestionKind,
    pub text: String,
    pub time_limit_sec: i64,
    pub explanation: Option<String>,
    pub options: Vec<NewOption>,
}

pub struct NewOption {
    pub id: Option<i64>,
    pub text: String,
    pub is_correct: bool,
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError::BadRequest(msg.into())
}

fn field(path: impl Into<String>, message: impl Into<String>) -> FieldError {
    FieldError {
        path: path.into(),
        message: message.into(),
    }
}

/// Check a whole quiz against the limits; every problem is reported, each
/// on the field it belongs to.
pub fn validate(input: &NewQuiz) -> AppResult<()> {
    use limits::*;
    let mut errs = Vec::new();
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        errs.push(field(
            "title",
            format!("must be 1-{MAX_TITLE_CHARS} characters"),
        ));
    }
    if input.description.trim().chars().count() > MAX_DESCRIPTION_CHARS {
        errs.push(field(
            "description",
            format!("must be at most {MAX_DESCRIPTION_CHARS} characters"),
        ));
    }
    if input.questions.is_empty() {
        errs.push(field("questions", "needs at least one question"));
    }
    if input.questions.len() > MAX_QUESTIONS {
        errs.push(field(
            "questions",
            format!("at most {MAX_QUESTIONS} questions"),
        ));
    }
    for (i, q) in input.questions.iter().enumerate().take(MAX_QUESTIONS + 1) {
        validate_question(&format!("questions[{i}]"), q, &mut errs);
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(AppError::Invalid(errs))
    }
}

fn validate_question(at: &str, q: &NewQuestion, errs: &mut Vec<FieldError>) {
    use limits::*;
    let text = q.text.trim();
    if text.is_empty() || text.chars().count() > MAX_QUESTION_CHARS {
        errs.push(field(
            format!("{at}.text"),
            format!("must be 1-{MAX_QUESTION_CHARS} characters"),
        ));
    }
    if !(MIN_TIME_LIMIT_SEC..=MAX_TIME_LIMIT_SEC).contains(&q.time_limit_sec) {
        errs.push(field(
            format!("{at}.timeLimitSec"),
            format!("must be {MIN_TIME_LIMIT_SEC}-{MAX_TIME_LIMIT_SEC} seconds"),
        ));
    }
    if let Some(e) = &q.explanation
        && e.trim().chars().count() > MAX_EXPLANATION_CHARS
    {
        errs.push(field(
            format!("{at}.explanation"),
            format!("must be at most {MAX_EXPLANATION_CHARS} characters"),
        ));
    }
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&q.options.len()) {
        errs.push(field(
            format!("{at}.options"),
            format!("needs {MIN_OPTIONS}-{MAX_OPTIONS} options"),
        ));
    }
    for (i, o) in q.options.iter().enumerate().take(MAX_OPTIONS + 1) {
        let t = o.text.trim();
        if t.is_empty() || t.chars().count() > MAX_OPTION_CHARS {
            errs.push(field(
                format!("{at}.options[{i}].text"),
                format!("must be 1-{MAX_OPTION_CHARS} characters"),
            ));
        }
    }
    let correct = q.options.iter().filter(|o| o.is_correct).count();
    match q.kind {
        QuestionKind::Boolean if q.options.len() != 2 => {
            errs.push(field(
                format!("{at}.options"),
                "a true/false question has exactly 2 options",
            ));
        }
        QuestionKind::Single | QuestionKind::Boolean if correct != 1 => {
            errs.push(field(
                format!("{at}.options"),
                "needs exactly one correct option",
            ));
        }
        QuestionKind::Multi if correct < 1 => {
            errs.push(field(
                format!("{at}.options"),
                "needs at least one correct option",
            ));
        }
        _ => {}
    }
}

fn trimmed(s: &str) -> &str {
    s.trim()
}

/// Create a quiz owned by `owner_id`, returning its id.
pub async fn create(
    conn: &mut SqliteConnection,
    owner_id: &str,
    input: &NewQuiz,
    keep: bool,
    now: i64,
) -> AppResult<i64> {
    validate(input)?;
    if input
        .questions
        .iter()
        .any(|q| q.id.is_some() || q.options.iter().any(|o| o.id.is_some()))
    {
        return Err(bad("a new quiz cannot carry question or option ids"));
    }
    let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quizzes WHERE owner_id = ?1")
        .bind(owner_id)
        .fetch_one(&mut *conn)
        .await?;
    if owned >= limits::MAX_QUIZZES_PER_OWNER {
        return Err(AppError::Conflict(format!(
            "at most {} quizzes per teacher",
            limits::MAX_QUIZZES_PER_OWNER
        )));
    }
    let quiz_id = new_id();
    sqlx::query(
        "INSERT INTO quizzes (id, owner_id, title, description, kept, expires_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?7, ?4, ?5, ?6, ?6)",
    )
    .bind(quiz_id)
    .bind(owner_id)
    .bind(trimmed(&input.title))
    .bind(keep)
    .bind(quiz_expiry(keep, now))
    .bind(now)
    .bind(trimmed(&input.description))
    .execute(&mut *conn)
    .await?;
    for (i, q) in input.questions.iter().enumerate() {
        insert_question(conn, quiz_id, i as i64 + 1, q).await?;
    }
    Ok(quiz_id)
}

async fn insert_question(
    conn: &mut SqliteConnection,
    quiz_id: i64,
    position: i64,
    q: &NewQuestion,
) -> AppResult<()> {
    let question_id = new_id();
    sqlx::query(
        "INSERT INTO questions (id, quiz_id, position, kind, text, time_limit_sec, explanation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(question_id)
    .bind(quiz_id)
    .bind(position)
    .bind(q.kind.as_str())
    .bind(trimmed(&q.text))
    .bind(q.time_limit_sec)
    .bind(
        q.explanation
            .as_deref()
            .map(trimmed)
            .filter(|e| !e.is_empty()),
    )
    .execute(&mut *conn)
    .await?;
    for (i, o) in q.options.iter().enumerate() {
        insert_option(conn, question_id, i as i64 + 1, o).await?;
    }
    Ok(())
}

async fn insert_option(
    conn: &mut SqliteConnection,
    question_id: i64,
    position: i64,
    o: &NewOption,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO options (id, question_id, position, text, is_correct)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(new_id())
    .bind(question_id)
    .bind(position)
    .bind(trimmed(&o.text))
    .bind(o.is_correct)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Replace the title and the whole question list. Questions and options that
/// carry an id are updated in place, the others are created, and those left
/// out are deleted (with their recorded answers). Counts as a use.
pub async fn update(
    conn: &mut SqliteConnection,
    quiz_id: i64,
    owner_id: &str,
    input: &NewQuiz,
    now: i64,
) -> AppResult<()> {
    validate(input)?;
    let quiz = owned(&mut *conn, quiz_id, owner_id).await?;
    sqlx::query(
        "UPDATE quizzes SET title = ?2, description = ?5, expires_at = ?3, updated_at = ?4 WHERE id = ?1",
    )
    .bind(quiz_id)
    .bind(trimmed(&input.title))
    .bind(quiz_expiry(quiz.kept, now))
    .bind(now)
    .bind(trimmed(&input.description))
    .execute(&mut *conn)
    .await?;

    let existing: HashSet<i64> = sqlx::query_scalar("SELECT id FROM questions WHERE quiz_id = ?1")
        .bind(quiz_id)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect();
    let mut given = HashSet::new();
    for (i, id) in input
        .questions
        .iter()
        .enumerate()
        .filter_map(|(i, q)| q.id.map(|id| (i, id)))
    {
        if !existing.contains(&id) || !given.insert(id) {
            return Err(AppError::Invalid(vec![field(
                format!("questions[{i}].id"),
                "does not belong to this quiz",
            )]));
        }
    }
    for gone in existing.difference(&given) {
        sqlx::query("DELETE FROM questions WHERE id = ?1")
            .bind(gone)
            .execute(&mut *conn)
            .await?;
    }
    for (i, q) in input.questions.iter().enumerate() {
        let position = i as i64 + 1;
        match q.id {
            None => insert_question(conn, quiz_id, position, q).await?,
            Some(id) => {
                sqlx::query(
                    "UPDATE questions SET position = ?2, kind = ?3, text = ?4,
                            time_limit_sec = ?5, explanation = ?6 WHERE id = ?1",
                )
                .bind(id)
                .bind(position)
                .bind(q.kind.as_str())
                .bind(trimmed(&q.text))
                .bind(q.time_limit_sec)
                .bind(
                    q.explanation
                        .as_deref()
                        .map(trimmed)
                        .filter(|e| !e.is_empty()),
                )
                .execute(&mut *conn)
                .await?;
                sync_options(conn, id, &q.options, i).await?;
            }
        }
    }
    Ok(())
}

async fn sync_options(
    conn: &mut SqliteConnection,
    question_id: i64,
    options: &[NewOption],
    at: usize,
) -> AppResult<()> {
    let existing: HashSet<i64> =
        sqlx::query_scalar("SELECT id FROM options WHERE question_id = ?1")
            .bind(question_id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    let mut given = HashSet::new();
    for (i, id) in options
        .iter()
        .enumerate()
        .filter_map(|(i, o)| o.id.map(|id| (i, id)))
    {
        if !existing.contains(&id) || !given.insert(id) {
            return Err(AppError::Invalid(vec![field(
                format!("questions[{at}].options[{i}].id"),
                "does not belong to this question",
            )]));
        }
    }
    for gone in existing.difference(&given) {
        sqlx::query("DELETE FROM options WHERE id = ?1")
            .bind(gone)
            .execute(&mut *conn)
            .await?;
    }
    for (i, o) in options.iter().enumerate() {
        let position = i as i64 + 1;
        match o.id {
            None => insert_option(conn, question_id, position, o).await?,
            Some(id) => {
                sqlx::query(
                    "UPDATE options SET position = ?2, text = ?3, is_correct = ?4 WHERE id = ?1",
                )
                .bind(id)
                .bind(position)
                .bind(trimmed(&o.text))
                .bind(o.is_correct)
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    Ok(())
}

/// Keep or let go of a quiz. Counts as a use: the expiry restarts with the
/// period that now applies.
pub async fn set_kept(
    conn: &mut SqliteConnection,
    quiz_id: i64,
    owner_id: &str,
    keep: bool,
    now: i64,
) -> AppResult<()> {
    let res = sqlx::query(
        "UPDATE quizzes SET kept = ?3, expires_at = ?4, updated_at = ?5
         WHERE id = ?1 AND owner_id = ?2",
    )
    .bind(quiz_id)
    .bind(owner_id)
    .bind(keep)
    .bind(quiz_expiry(keep, now))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("quiz".into()));
    }
    Ok(())
}

/// Delete a quiz and, by cascade, its games. Immediate and permanent.
pub async fn delete(conn: &mut SqliteConnection, quiz_id: i64, owner_id: &str) -> AppResult<()> {
    let res = sqlx::query("DELETE FROM quizzes WHERE id = ?1 AND owner_id = ?2")
        .bind(quiz_id)
        .bind(owner_id)
        .execute(&mut *conn)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("quiz".into()));
    }
    Ok(())
}

/// The quiz when `owner_id` owns it; anyone else gets `NotFound`, so ids of
/// other people's quizzes are not confirmed to exist.
pub async fn owned<'e, E>(exec: E, quiz_id: i64, owner_id: &str) -> AppResult<QuizRow>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    sqlx::query_as::<_, QuizRow>(&format!(
        "SELECT {QUIZ_COLS} FROM quizzes WHERE id = ?1 AND owner_id = ?2"
    ))
    .bind(quiz_id)
    .bind(owner_id)
    .fetch_optional(exec)
    .await?
    .ok_or_else(|| AppError::NotFound("quiz".into()))
}

/// One page of an owner's quizzes, newest first, with the total.
pub async fn page<'e, E>(
    exec: E,
    owner_id: &str,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<QuizRow>, i64)>
where
    E: 'e + Send + Clone + Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query_as::<_, QuizRow>(&format!(
        "SELECT {QUIZ_COLS} FROM quizzes WHERE owner_id = ?1
         ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3"
    ))
    .bind(owner_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(exec.clone())
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quizzes WHERE owner_id = ?1")
        .bind(owner_id)
        .fetch_one(exec)
        .await?;
    Ok((rows, total))
}

/// Questions with their options, in order.
pub async fn questions<'e, E>(exec: E, quiz_id: i64) -> AppResult<Vec<QuestionWithOptions>>
where
    E: 'e + Send + Clone + Executor<'e, Database = Sqlite>,
{
    let questions = sqlx::query_as::<_, QuestionRow>(
        "SELECT id, quiz_id, position, kind, text, time_limit_sec, explanation
         FROM questions WHERE quiz_id = ?1 ORDER BY position, id",
    )
    .bind(quiz_id)
    .fetch_all(exec.clone())
    .await?;
    let options = sqlx::query_as::<_, OptionRow>(
        "SELECT o.id, o.question_id, o.position, o.text, o.is_correct
         FROM options o JOIN questions q ON q.id = o.question_id
         WHERE q.quiz_id = ?1 ORDER BY o.question_id, o.position, o.id",
    )
    .bind(quiz_id)
    .fetch_all(exec)
    .await?;
    let mut by_question: HashMap<i64, Vec<OptionRow>> = HashMap::new();
    for o in options {
        by_question.entry(o.question_id).or_default().push(o);
    }
    Ok(questions
        .into_iter()
        .map(|q| QuestionWithOptions {
            options: by_question.remove(&q.id).unwrap_or_default(),
            question: q,
        })
        .collect())
}

/// Question counts for a page of quizzes.
pub async fn question_counts<'e, E>(exec: E, quiz_ids: &[i64]) -> AppResult<HashMap<i64, i64>>
where
    E: 'e + Send + Clone + Executor<'e, Database = Sqlite>,
{
    let mut out = HashMap::new();
    for chunk in quiz_ids.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT quiz_id, COUNT(*) FROM questions WHERE quiz_id IN ({marks}) GROUP BY quiz_id"
        );
        let mut q = sqlx::query_as::<_, (i64, i64)>(&sql);
        for id in chunk {
            q = q.bind(id);
        }
        out.extend(q.fetch_all(exec.clone()).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(kind: QuestionKind, options: &[bool]) -> NewQuestion {
        NewQuestion {
            id: None,
            kind,
            text: "What?".into(),
            time_limit_sec: 20,
            explanation: None,
            options: options
                .iter()
                .map(|c| NewOption {
                    id: None,
                    text: "o".into(),
                    is_correct: *c,
                })
                .collect(),
        }
    }

    fn quiz(questions: Vec<NewQuestion>) -> NewQuiz {
        NewQuiz {
            title: "T".into(),
            description: String::new(),
            questions,
        }
    }

    #[test]
    fn accepts_the_three_kinds() {
        let q = quiz(vec![
            question(QuestionKind::Single, &[true, false, false]),
            question(QuestionKind::Multi, &[true, true, false]),
            question(QuestionKind::Boolean, &[false, true]),
        ]);
        assert!(validate(&q).is_ok());
    }

    #[test]
    fn enforces_every_limit() {
        use limits::*;
        let ok = || question(QuestionKind::Single, &[true, false]);
        let cases: Vec<(&str, NewQuiz)> = vec![
            (
                "empty title",
                NewQuiz {
                    title: "  ".into(),
                    description: String::new(),
                    questions: vec![ok()],
                },
            ),
            (
                "long title",
                NewQuiz {
                    title: "x".repeat(MAX_TITLE_CHARS + 1),
                    description: String::new(),
                    questions: vec![ok()],
                },
            ),
            ("no questions", quiz(vec![])),
            (
                "101 questions",
                quiz((0..=MAX_QUESTIONS).map(|_| ok()).collect()),
            ),
            (
                "long question",
                quiz(vec![NewQuestion {
                    text: "x".repeat(MAX_QUESTION_CHARS + 1),
                    ..ok()
                }]),
            ),
            (
                "short time",
                quiz(vec![NewQuestion {
                    time_limit_sec: 4,
                    ..ok()
                }]),
            ),
            (
                "long time",
                quiz(vec![NewQuestion {
                    time_limit_sec: 121,
                    ..ok()
                }]),
            ),
            (
                "one option",
                quiz(vec![question(QuestionKind::Single, &[true])]),
            ),
            (
                "seven options",
                quiz(vec![question(QuestionKind::Multi, &[true; 7])]),
            ),
            (
                "three-option boolean",
                quiz(vec![question(QuestionKind::Boolean, &[true, false, false])]),
            ),
            (
                "two correct singles",
                quiz(vec![question(QuestionKind::Single, &[true, true])]),
            ),
            (
                "no correct multi",
                quiz(vec![question(QuestionKind::Multi, &[false, false])]),
            ),
            (
                "long option",
                quiz(vec![NewQuestion {
                    options: vec![
                        NewOption {
                            id: None,
                            text: "x".repeat(MAX_OPTION_CHARS + 1),
                            is_correct: true,
                        },
                        NewOption {
                            id: None,
                            text: "b".into(),
                            is_correct: false,
                        },
                    ],
                    ..ok()
                }]),
            ),
        ];
        for (name, q) in cases {
            assert!(validate(&q).is_err(), "{name} should be rejected");
        }
        let edge = quiz(
            (0..MAX_QUESTIONS)
                .map(|_| NewQuestion {
                    time_limit_sec: 120,
                    ..ok()
                })
                .collect(),
        );
        assert!(validate(&edge).is_ok(), "100 questions at 120 s is allowed");
    }
}
