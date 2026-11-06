//! Domain rows (1:1 with tables).

use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;

/// The question kind stored in `questions.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type, Serialize, Deserialize, ToSchema)]
#[sqlx(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum QuestionKind {
    Single,
    Multi,
    Boolean,
}

impl QuestionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuestionKind::Single => "single",
            QuestionKind::Multi => "multi",
            QuestionKind::Boolean => "boolean",
        }
    }
}

#[derive(Clone, Debug, FromRow)]
pub struct QuizRow {
    pub id: i64,
    pub owner_id: String,
    pub title: String,
    pub description: String,
    pub kept: bool,
    pub expires_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, FromRow)]
pub struct QuestionRow {
    pub id: i64,
    pub quiz_id: i64,
    pub position: i64,
    pub kind: QuestionKind,
    pub text: String,
    pub time_limit_sec: i64,
    pub explanation: Option<String>,
}

#[derive(Clone, Debug, FromRow)]
pub struct OptionRow {
    pub id: i64,
    pub question_id: i64,
    pub position: i64,
    pub text: String,
    pub is_correct: bool,
}

/// A question together with its options, as needed for grading.
#[derive(Clone, Debug)]
pub struct QuestionWithOptions {
    pub question: QuestionRow,
    pub options: Vec<OptionRow>,
}
