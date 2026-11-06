//! Request and response bodies of the bank API. Ids are strings (the
//! service's 62-bit ids do not fit a JSON number); times are unix epoch
//! milliseconds.

use crate::model::{QuestionKind, QuestionWithOptions, QuizRow};
use crate::repo::games::GameResult as GameResultRow;
use crate::repo::quizzes::{NewOption, NewQuestion, NewQuiz, limits::DEFAULT_TIME_LIMIT_SEC};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

fn default_true() -> bool {
    true
}

fn default_time_limit() -> i64 {
    DEFAULT_TIME_LIMIT_SEC
}

/// One quiz in a list.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuizSummary {
    pub id: String,
    pub title: String,
    pub description: String,
    /// Kept quizzes live 90 days after their last use, the others 7.
    pub kept: bool,
    /// When the sweep deletes the quiz unless it is used first.
    pub expires_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub question_count: i64,
}

impl QuizSummary {
    pub fn new(row: &QuizRow, question_count: i64) -> Self {
        QuizSummary {
            id: row.id.to_string(),
            title: row.title.clone(),
            description: row.description.clone(),
            kept: row.kept,
            expires_at: row.expires_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            question_count,
        }
    }
}

/// A page of the bank.
#[derive(Serialize, ToSchema)]
pub struct QuizList {
    pub items: Vec<QuizSummary>,
    /// Quizzes in the whole bank.
    pub total: i64,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OptionOut {
    pub id: String,
    pub text: String,
    pub is_correct: bool,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOut {
    pub id: String,
    pub kind: QuestionKind,
    pub text: String,
    pub time_limit_sec: i64,
    pub explanation: Option<String>,
    /// In the order shown to players.
    pub options: Vec<OptionOut>,
}

/// A quiz with its questions and answer keys (its owner only).
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Quiz {
    pub id: String,
    pub title: String,
    pub description: String,
    pub kept: bool,
    pub expires_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub question_count: i64,
    /// In the order they are asked.
    pub questions: Vec<QuestionOut>,
}

impl Quiz {
    pub fn new(row: &QuizRow, questions: Vec<QuestionWithOptions>) -> Self {
        Quiz {
            id: row.id.to_string(),
            title: row.title.clone(),
            description: row.description.clone(),
            kept: row.kept,
            expires_at: row.expires_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            question_count: questions.len() as i64,
            questions: questions
                .into_iter()
                .map(|q| QuestionOut {
                    id: q.question.id.to_string(),
                    kind: q.question.kind,
                    text: q.question.text,
                    time_limit_sec: q.question.time_limit_sec,
                    explanation: q.question.explanation,
                    options: q
                        .options
                        .into_iter()
                        .map(|o| OptionOut {
                            id: o.id.to_string(),
                            text: o.text,
                            is_correct: o.is_correct,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OptionIn {
    /// Give the id of an existing option to keep it, leave out for a new one.
    pub id: Option<String>,
    pub text: String,
    pub is_correct: bool,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuestionIn {
    /// Give the id of an existing question to keep it (and the results
    /// recorded for it), leave out for a new one.
    pub id: Option<String>,
    pub kind: QuestionKind,
    pub text: String,
    /// 5 to 120 seconds.
    #[serde(default = "default_time_limit")]
    #[schema(default = 20)]
    pub time_limit_sec: i64,
    pub explanation: Option<String>,
    /// 2 to 6 options; exactly 2 for a true/false question.
    pub options: Vec<OptionIn>,
}

/// Body of `POST /v1/quizzes`.
#[derive(Deserialize, ToSchema)]
pub struct CreateQuizBody {
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Keep the quiz (default) or let it go after 7 days.
    #[serde(default = "default_true")]
    #[schema(default = true)]
    pub kept: bool,
    /// 1 to 100 questions.
    pub questions: Vec<QuestionIn>,
}

/// Body of `PUT /v1/quizzes/{id}`: the whole quiz, saved at once.
#[derive(Deserialize, ToSchema)]
pub struct UpdateQuizBody {
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// 1 to 100 questions; those left out are deleted.
    pub questions: Vec<QuestionIn>,
}

/// Body of `PUT /v1/quizzes/{id}/kept`.
#[derive(Deserialize, ToSchema)]
pub struct KeptBody {
    pub kept: bool,
}

/// A player's line on a finished game's scoreboard.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlayerResult {
    pub rank: i64,
    /// Empty once the person has been forgotten.
    pub name: String,
    pub anonymous: bool,
    pub score: i64,
    pub total_ms: i64,
    pub correct_count: i64,
    pub answered_count: i64,
}

/// How a question went in a finished game.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuestionResult {
    pub question_id: String,
    pub position: i64,
    pub text: String,
    pub answered_count: i64,
    pub correct_count: i64,
}

/// A finished game's results.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GameResult {
    pub id: String,
    pub started_at: i64,
    pub ended_at: i64,
    /// When the results are deleted (90 days after the game ended).
    pub expires_at: i64,
    /// Best first.
    pub players: Vec<PlayerResult>,
    pub questions: Vec<QuestionResult>,
}

impl From<GameResultRow> for GameResult {
    fn from(g: GameResultRow) -> Self {
        GameResult {
            id: g.id.to_string(),
            started_at: g.started_at,
            ended_at: g.ended_at,
            expires_at: g.expires_at,
            players: g
                .players
                .into_iter()
                .map(|p| PlayerResult {
                    rank: p.rank,
                    name: p.name,
                    anonymous: p.anonymous,
                    score: p.score,
                    total_ms: p.total_ms,
                    correct_count: p.correct_count,
                    answered_count: p.answered_count,
                })
                .collect(),
            questions: g
                .questions
                .into_iter()
                .map(|q| QuestionResult {
                    question_id: q.question_id.to_string(),
                    position: q.position,
                    text: q.text,
                    answered_count: q.answered_count,
                    correct_count: q.correct_count,
                })
                .collect(),
        }
    }
}

fn parse_opt_id(
    id: &Option<String>,
    path: String,
) -> Result<Option<i64>, crate::error::FieldError> {
    match id {
        None => Ok(None),
        Some(s) => s.parse().map(Some).map_err(|_| crate::error::FieldError {
            path,
            message: "is not an id".into(),
        }),
    }
}

/// Turn a request's questions into the repository's input; bad ids are
/// reported on their field.
pub fn new_quiz(
    title: String,
    description: String,
    questions: Vec<QuestionIn>,
) -> crate::error::AppResult<NewQuiz> {
    let mut errs = Vec::new();
    let mut out = Vec::with_capacity(questions.len());
    for (i, q) in questions.into_iter().enumerate() {
        let id = parse_opt_id(&q.id, format!("questions[{i}].id")).unwrap_or_else(|e| {
            errs.push(e);
            None
        });
        let mut options = Vec::with_capacity(q.options.len());
        for (j, o) in q.options.into_iter().enumerate() {
            let oid = parse_opt_id(&o.id, format!("questions[{i}].options[{j}].id"))
                .unwrap_or_else(|e| {
                    errs.push(e);
                    None
                });
            options.push(NewOption {
                id: oid,
                text: o.text,
                is_correct: o.is_correct,
            });
        }
        out.push(NewQuestion {
            id,
            kind: q.kind,
            text: q.text,
            time_limit_sec: q.time_limit_sec,
            explanation: q.explanation,
            options,
        });
    }
    if !errs.is_empty() {
        return Err(crate::error::AppError::Invalid(errs));
    }
    Ok(NewQuiz {
        title,
        description,
        questions: out,
    })
}
