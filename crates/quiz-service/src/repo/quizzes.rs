//! Quiz / question / option repository.

use crate::error::{AppError, AppResult};
use crate::ids::{new_id, now_ms};
use crate::model::{OptionRow, QuestionKind, QuestionRow, QuestionWithOptions, QuizRow};
use sqlx::{Executor, Sqlite};

const QUIZ_COLS: &str = "id, creator_id, title, description, is_public, time_limit_sec, max_attempts, created_at, updated_at";

/// Payload for creating a quiz.
pub struct NewQuiz {
    pub title: String,
    pub description: String,
    pub is_public: bool,
    pub time_limit_sec: Option<i64>,
    pub max_attempts: Option<i64>,
}

/// Payload for one option of a new question.
pub struct NewOption {
    pub text: String,
    pub is_correct: bool,
}

/// Payload for creating a question (with its options).
pub struct NewQuestion {
    pub kind: QuestionKind,
    pub text: String,
    pub points: f64,
    pub explanation: Option<String>,
    pub options: Vec<NewOption>,
}

pub async fn create_quiz(
    conn: &mut sqlx::SqliteConnection,
    creator_id: i64,
    input: NewQuiz,
) -> AppResult<QuizRow> {
    validate_quiz_input(&input)?;
    let now = now_ms();
    let quiz = sqlx::query_as::<_, QuizRow>(&format!(
        "INSERT INTO quizzes ({QUIZ_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         RETURNING {QUIZ_COLS}"
    ))
    .bind(new_id())
    .bind(creator_id)
    .bind(input.title.trim())
    .bind(input.description.trim())
    .bind(input.is_public)
    .bind(input.time_limit_sec)
    .bind(input.max_attempts)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    Ok(quiz)
}

fn validate_quiz_input(input: &NewQuiz) -> AppResult<()> {
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > 140 {
        return Err(AppError::BadRequest(
            "quiz title must be 1-140 characters".into(),
        ));
    }
    if input.description.chars().count() > 2000 {
        return Err(AppError::BadRequest(
            "quiz description too long (max 2000)".into(),
        ));
    }
    if let Some(tl) = input.time_limit_sec
        && !(1..=86_400).contains(&tl)
    {
        return Err(AppError::BadRequest(
            "time_limit_sec must be 1-86400".into(),
        ));
    }
    if let Some(ma) = input.max_attempts
        && !(1..=1000).contains(&ma)
    {
        return Err(AppError::BadRequest("max_attempts must be 1-1000".into()));
    }
    Ok(())
}

pub async fn by_id<'e, E>(exec: E, id: i64) -> AppResult<Option<QuizRow>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let quiz =
        sqlx::query_as::<_, QuizRow>(&format!("SELECT {QUIZ_COLS} FROM quizzes WHERE id = ?1"))
            .bind(id)
            .fetch_optional(exec)
            .await?;
    Ok(quiz)
}

/// Public quizzes (plus the viewer's own private ones), newest first.
pub async fn list_page<'e, E>(
    exec: E,
    viewer_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<QuizRow>, i64)>
where
    E: 'e + Send + Clone + Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query_as::<_, QuizRow>(&format!(
        "SELECT {QUIZ_COLS} FROM quizzes
         WHERE is_public = 1 OR creator_id = ?1
         ORDER BY created_at DESC, id DESC
         LIMIT ?2 OFFSET ?3"
    ))
    .bind(viewer_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(exec.clone())
    .await?;

    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM quizzes WHERE is_public = 1 OR creator_id = ?1")
            .bind(viewer_id)
            .fetch_one(exec)
            .await?;

    Ok((rows, total))
}

pub async fn update_quiz(
    conn: &mut sqlx::SqliteConnection,
    quiz_id: i64,
    caller_id: i64,
    input: NewQuiz,
) -> AppResult<QuizRow> {
    validate_quiz_input(&input)?;
    let quiz = sqlx::query_as::<_, QuizRow>(&format!(
        "UPDATE quizzes SET title = ?3, description = ?4, is_public = ?5, time_limit_sec = ?6,
                max_attempts = ?7, updated_at = ?8
         WHERE id = ?1 AND creator_id = ?2
         RETURNING {QUIZ_COLS}"
    ))
    .bind(quiz_id)
    .bind(caller_id)
    .bind(input.title.trim())
    .bind(input.description.trim())
    .bind(input.is_public)
    .bind(input.time_limit_sec)
    .bind(input.max_attempts)
    .bind(now_ms())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::NotFound("quiz".into()))?;
    Ok(quiz)
}

pub async fn delete_quiz(
    conn: &mut sqlx::SqliteConnection,
    quiz_id: i64,
    caller_id: i64,
) -> AppResult<()> {
    let res = sqlx::query("DELETE FROM quizzes WHERE id = ?1 AND creator_id = ?2")
        .bind(quiz_id)
        .bind(caller_id)
        .execute(&mut *conn)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("quiz".into()));
    }
    Ok(())
}

/// Validate and insert a batch of questions with their options.
pub async fn add_questions(
    conn: &mut sqlx::SqliteConnection,
    quiz_id: i64,
    caller_id: i64,
    questions: &[NewQuestion],
) -> AppResult<Vec<QuestionRow>> {
    if questions.is_empty() {
        return Err(AppError::BadRequest(
            "at least one question is required".into(),
        ));
    }
    if questions.len() > 200 {
        return Err(AppError::BadRequest(
            "at most 200 questions per batch".into(),
        ));
    }

    // Ownership check inside the transaction.
    let owner: Option<i64> = sqlx::query_scalar("SELECT creator_id FROM quizzes WHERE id = ?1")
        .bind(quiz_id)
        .fetch_optional(&mut *conn)
        .await?;
    match owner {
        None => return Err(AppError::NotFound("quiz".into())),
        Some(o) if o != caller_id => return Err(AppError::Forbidden),
        _ => {}
    }

    for q in questions {
        validate_question(q)?;
    }

    let next_pos: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(position), 0) FROM questions WHERE quiz_id = ?1")
            .bind(quiz_id)
            .fetch_one(&mut *conn)
            .await?;

    let mut created = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        let qid = new_id();
        let row = sqlx::query_as::<_, QuestionRow>(
            "INSERT INTO questions (id, quiz_id, position, kind, text, points, explanation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING id, quiz_id, position, kind, text, points, explanation",
        )
        .bind(qid)
        .bind(quiz_id)
        .bind(next_pos + i as i64 + 1)
        .bind(q.kind.as_str())
        .bind(q.text.trim())
        .bind(q.points)
        .bind(&q.explanation)
        .fetch_one(&mut *conn)
        .await?;

        for (opos, o) in q.options.iter().enumerate() {
            sqlx::query(
                "INSERT INTO options (id, question_id, position, text, is_correct)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(new_id())
            .bind(qid)
            .bind(opos as i64 + 1)
            .bind(o.text.trim())
            .bind(o.is_correct)
            .execute(&mut *conn)
            .await?;
        }
        created.push(row);
    }
    Ok(created)
}

fn validate_question(q: &NewQuestion) -> AppResult<()> {
    let text = q.text.trim();
    if text.is_empty() || text.chars().count() > 2000 {
        return Err(AppError::BadRequest(
            "question text must be 1-2000 characters".into(),
        ));
    }
    if !(0.5..=100.0).contains(&q.points) {
        return Err(AppError::BadRequest(
            "points must be between 0.5 and 100".into(),
        ));
    }
    if q.options.len() < 2 || q.options.len() > 10 {
        return Err(AppError::BadRequest("questions need 2-10 options".into()));
    }
    for o in &q.options {
        if o.text.trim().is_empty() {
            return Err(AppError::BadRequest("option text must not be empty".into()));
        }
    }
    let correct = q.options.iter().filter(|o| o.is_correct).count();
    match q.kind {
        QuestionKind::Single | QuestionKind::Boolean => {
            if q.kind == QuestionKind::Boolean && q.options.len() != 2 {
                return Err(AppError::BadRequest(
                    "boolean questions need exactly 2 options".into(),
                ));
            }
            if correct != 1 {
                return Err(AppError::BadRequest(
                    "single/boolean questions need exactly one correct option".into(),
                ));
            }
        }
        QuestionKind::Multi => {
            if correct < 1 {
                return Err(AppError::BadRequest(
                    "multi questions need at least one correct option".into(),
                ));
            }
        }
    }
    Ok(())
}

pub async fn update_question(
    conn: &mut sqlx::SqliteConnection,
    question_id: i64,
    caller_id: i64,
    q: NewQuestion,
) -> AppResult<QuestionRow> {
    validate_question(&q)?;
    let quiz_creator: Option<i64> = sqlx::query_scalar(
        "SELECT qz.creator_id FROM questions q JOIN quizzes qz ON qz.id = q.quiz_id WHERE q.id = ?1",
    )
    .bind(question_id)
    .fetch_optional(&mut *conn)
    .await?;
    match quiz_creator {
        None => return Err(AppError::NotFound("question".into())),
        Some(c) if c != caller_id => return Err(AppError::Forbidden),
        _ => {}
    }

    let row = sqlx::query_as::<_, QuestionRow>(
        "UPDATE questions SET kind = ?2, text = ?3, points = ?4, explanation = ?5
         WHERE id = ?1
         RETURNING id, quiz_id, position, kind, text, points, explanation",
    )
    .bind(question_id)
    .bind(q.kind.as_str())
    .bind(q.text.trim())
    .bind(q.points)
    .bind(&q.explanation)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::NotFound("question".into()))?;

    sqlx::query("DELETE FROM options WHERE question_id = ?1")
        .bind(question_id)
        .execute(&mut *conn)
        .await?;
    for (opos, o) in q.options.iter().enumerate() {
        sqlx::query(
            "INSERT INTO options (id, question_id, position, text, is_correct)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(new_id())
        .bind(question_id)
        .bind(opos as i64 + 1)
        .bind(o.text.trim())
        .bind(o.is_correct)
        .execute(&mut *conn)
        .await?;
    }
    Ok(row)
}

pub async fn delete_question(
    conn: &mut sqlx::SqliteConnection,
    question_id: i64,
    caller_id: i64,
) -> AppResult<()> {
    let res = sqlx::query(
        "DELETE FROM questions WHERE id = ?1
         AND EXISTS (SELECT 1 FROM quizzes qz WHERE qz.id = questions.quiz_id AND qz.creator_id = ?2)",
    )
    .bind(question_id)
    .bind(caller_id)
    .execute(&mut *conn)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("question".into()));
    }
    Ok(())
}

/// Questions of a quiz ordered by position (single executor use).
pub async fn quiz_questions<'e, E>(exec: E, quiz_id: i64) -> AppResult<Vec<QuestionRow>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let questions = sqlx::query_as::<_, QuestionRow>(
        "SELECT id, quiz_id, position, kind, text, points, explanation
         FROM questions WHERE quiz_id = ?1 ORDER BY position",
    )
    .bind(quiz_id)
    .fetch_all(exec)
    .await?;
    Ok(questions)
}

/// All options of a quiz's questions (single executor use).
pub async fn quiz_options<'e, E>(exec: E, quiz_id: i64) -> AppResult<Vec<OptionRow>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let options = sqlx::query_as::<_, OptionRow>(
        "SELECT o.id, o.question_id, o.position, o.text, o.is_correct
         FROM options o JOIN questions q ON q.id = o.question_id
         WHERE q.quiz_id = ?1 ORDER BY o.question_id, o.position",
    )
    .bind(quiz_id)
    .fetch_all(exec)
    .await?;
    Ok(options)
}

/// Group options under their questions (pure, no executor).
pub fn assemble_questions(
    questions: Vec<QuestionRow>,
    options: Vec<OptionRow>,
) -> Vec<QuestionWithOptions> {
    let mut by_question: std::collections::HashMap<i64, Vec<OptionRow>> =
        std::collections::HashMap::new();
    for o in options {
        by_question.entry(o.question_id).or_default().push(o);
    }
    questions
        .into_iter()
        .map(|q| {
            let opts = by_question.remove(&q.id).unwrap_or_default();
            QuestionWithOptions {
                question: q,
                options: opts,
            }
        })
        .collect()
}

/// Load all questions with options of a quiz (pool convenience wrapper).
pub async fn questions_with_options<'e, E>(
    exec: E,
    quiz_id: i64,
) -> AppResult<Vec<QuestionWithOptions>>
where
    E: 'e + Send + Clone + Executor<'e, Database = Sqlite>,
{
    let questions = quiz_questions(exec.clone(), quiz_id).await?;
    if questions.is_empty() {
        return Ok(Vec::new());
    }
    let options = quiz_options(exec, quiz_id).await?;
    Ok(assemble_questions(questions, options))
}

pub async fn question_count<'e, E>(exec: E, quiz_id: i64) -> AppResult<i64>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM questions WHERE quiz_id = ?1")
        .bind(quiz_id)
        .fetch_one(exec)
        .await?;
    Ok(n)
}

/// Options of a single question, ordered by position.
pub async fn options_for_question<'e, E>(exec: E, question_id: i64) -> AppResult<Vec<OptionRow>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query_as::<_, OptionRow>(
        "SELECT id, question_id, position, text, is_correct
         FROM options WHERE question_id = ?1 ORDER BY position",
    )
    .bind(question_id)
    .fetch_all(exec)
    .await?;
    Ok(rows)
}

/// Question counts for many quizzes in one query (`quiz_id -> count`).
pub async fn count_many<'e, E>(
    exec: E,
    quiz_ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, i64>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite> + Clone,
{
    let mut out = std::collections::HashMap::new();
    if quiz_ids.is_empty() {
        return Ok(out);
    }
    // Chunk to stay far below SQLITE_MAX_VARIABLE_NUMBER.
    for chunk in quiz_ids.chunks(500) {
        let placeholders = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT quiz_id, COUNT(*) FROM questions WHERE quiz_id IN ({placeholders}) GROUP BY quiz_id"
        );
        let mut q = sqlx::query_as::<_, (i64, i64)>(&sql);
        for id in chunk {
            q = q.bind(id);
        }
        for (quiz_id, count) in q.fetch_all(exec.clone()).await? {
            out.insert(quiz_id, count);
        }
    }
    Ok(out)
}

/// Fetch a quiz and verify the viewer may read it (public or owner).
pub async fn visible_quiz(
    pool: &sqlx::SqlitePool,
    quiz_id: i64,
    viewer: Option<i64>,
) -> AppResult<QuizRow> {
    let quiz = by_id(pool, quiz_id)
        .await?
        .ok_or_else(|| AppError::NotFound("quiz".into()))?;
    if !quiz.is_public && Some(quiz.creator_id) != viewer {
        return Err(AppError::Forbidden);
    }
    Ok(quiz)
}
