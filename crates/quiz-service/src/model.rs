//! Domain rows (1:1 with tables) and shared event payloads.

use sqlx::FromRow;

/// The quiz "kind" stored in `questions.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "lowercase")]
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

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "single" => Some(QuestionKind::Single),
            "multi" => Some(QuestionKind::Multi),
            "boolean" => Some(QuestionKind::Boolean),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, FromRow)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, FromRow)]
pub struct QuizRow {
    pub id: i64,
    pub creator_id: i64,
    pub title: String,
    pub description: String,
    pub is_public: bool,
    pub time_limit_sec: Option<i64>,
    pub max_attempts: Option<i64>,
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
    pub points: f64,
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

#[derive(Clone, Debug, FromRow)]
pub struct AttemptRow {
    pub id: i64,
    pub quiz_id: i64,
    pub user_id: i64,
    pub status: String,
    pub started_at: i64,
    pub submitted_at: Option<i64>,
    pub score: Option<f64>,
    pub max_score: Option<f64>,
}

#[derive(Clone, Debug, FromRow)]
pub struct AnswerRow {
    pub id: i64,
    pub attempt_id: i64,
    pub question_id: i64,
    pub selected: String,
    pub is_correct: bool,
    pub awarded: f64,
    pub possible: f64,
}

/// A question together with its options, as needed for grading.
#[derive(Clone, Debug)]
pub struct QuestionWithOptions {
    pub question: QuestionRow,
    pub options: Vec<OptionRow>,
}

/// Broadcast payload for the `leaderboardUpdated` subscription.
#[derive(Clone, Debug)]
pub struct LeaderboardEvent {
    pub quiz_id: i64,
    /// Quiz title, for convenience on clients.
    pub quiz_title: String,
    /// Fresh top-N entries at publish time.
    pub entries: Vec<LeaderboardEntry>,
}

#[derive(Clone, Debug, FromRow)]
pub struct LeaderboardEntry {
    pub user_id: i64,
    pub username: String,
    pub score: f64,
    pub max_score: f64,
    pub submitted_at: i64,
    /// 1-based rank, assigned by the query wrapper.
    pub rank: i64,
}
