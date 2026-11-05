//! GraphQL object & input types (the public API surface).

use crate::model::{
    AttemptRow, LeaderboardEntry, LeaderboardEvent, OptionRow, QuestionKind, QuestionRow, QuizRow,
    UserRow,
};
use async_graphql::{Enum, ID, InputObject, SimpleObject};

pub fn gql_id(v: i64) -> ID {
    ID(v.to_string())
}

pub fn parse_id(id: &ID) -> crate::error::AppResult<i64> {
    id.parse::<i64>()
        .map_err(|_| crate::error::AppError::BadRequest(format!("invalid id `{id:?}`")))
}

/// Question kinds understood by the grading engine.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
#[graphql(name = "QuestionKind")]
pub enum QuestionKindG {
    Single,
    Multi,
    Boolean,
}

impl From<QuestionKind> for QuestionKindG {
    fn from(k: QuestionKind) -> Self {
        match k {
            QuestionKind::Single => QuestionKindG::Single,
            QuestionKind::Multi => QuestionKindG::Multi,
            QuestionKind::Boolean => QuestionKindG::Boolean,
        }
    }
}

impl From<QuestionKindG> for QuestionKind {
    fn from(k: QuestionKindG) -> Self {
        match k {
            QuestionKindG::Single => QuestionKind::Single,
            QuestionKindG::Multi => QuestionKind::Multi,
            QuestionKindG::Boolean => QuestionKind::Boolean,
        }
    }
}

/// Minimal user reference (used inside quiz/leaderboard payloads).
#[derive(SimpleObject, Clone, Debug)]
#[graphql(name = "UserSummary")]
pub struct UserRefG {
    pub id: ID,
    pub username: String,
}

#[derive(SimpleObject, Debug)]
#[graphql(name = "User")]
pub struct UserG {
    pub id: ID,
    pub username: String,
    /// Unix epoch milliseconds.
    pub created_at: i64,
}

impl From<UserRow> for UserG {
    fn from(u: UserRow) -> Self {
        UserG {
            id: gql_id(u.id),
            username: u.username,
            created_at: u.created_at,
        }
    }
}

impl From<UserRow> for UserRefG {
    fn from(u: UserRow) -> Self {
        UserRefG {
            id: gql_id(u.id),
            username: u.username,
        }
    }
}

#[derive(SimpleObject, Debug)]
#[graphql(name = "AuthPayload")]
pub struct AuthPayloadG {
    pub token: String,
    pub user: UserG,
}

#[derive(SimpleObject, Clone, Debug)]
#[graphql(name = "Quiz")]
pub struct QuizG {
    pub id: ID,
    pub title: String,
    pub description: String,
    pub is_public: bool,
    pub time_limit_sec: Option<i64>,
    pub max_attempts: Option<i64>,
    pub question_count: i64,
    /// Unix epoch milliseconds.
    pub created_at: i64,
    pub updated_at: i64,
    pub creator: UserRefG,
}

/// A quiz plus its questions (options hide correctness unless the viewer is
/// the quiz creator).
#[derive(SimpleObject, Debug)]
#[graphql(name = "QuizDetail")]
pub struct QuizDetailG {
    pub quiz: QuizG,
    pub questions: Vec<QuestionG>,
}

#[derive(SimpleObject, Clone, Debug)]
#[graphql(name = "QuizOption")]
pub struct OptionG {
    pub id: ID,
    pub position: i64,
    pub text: String,
    /// `null` when correctness is hidden from this viewer (non-creator).
    pub is_correct: Option<bool>,
}

#[derive(SimpleObject, Clone, Debug)]
#[graphql(name = "Question")]
pub struct QuestionG {
    pub id: ID,
    pub position: i64,
    pub kind: QuestionKindG,
    pub text: String,
    pub points: f64,
    pub explanation: Option<String>,
    pub options: Vec<OptionG>,
}

#[derive(SimpleObject, Debug)]
#[graphql(name = "QuizPage")]
pub struct QuizPageG {
    pub items: Vec<QuizG>,
    pub total: i64,
}

#[derive(SimpleObject, Clone, Debug)]
#[graphql(name = "Attempt")]
pub struct AttemptG {
    pub id: ID,
    pub quiz_id: ID,
    pub status: String,
    /// Unix epoch milliseconds.
    pub started_at: i64,
    pub submitted_at: Option<i64>,
    pub score: Option<f64>,
    pub max_score: Option<f64>,
}

impl From<AttemptRow> for AttemptG {
    fn from(a: AttemptRow) -> Self {
        AttemptG {
            id: gql_id(a.id),
            quiz_id: gql_id(a.quiz_id),
            status: a.status,
            started_at: a.started_at,
            submitted_at: a.submitted_at,
            score: a.score,
            max_score: a.max_score,
        }
    }
}

#[derive(SimpleObject, Clone, Debug)]
#[graphql(name = "GradedAnswer")]
pub struct GradedAnswerG {
    pub question_id: ID,
    pub selected_option_ids: Vec<ID>,
    pub correct_option_ids: Vec<ID>,
    pub correct: bool,
    pub awarded: f64,
    pub possible: f64,
}

impl From<crate::grading::GradedAnswer> for GradedAnswerG {
    fn from(a: crate::grading::GradedAnswer) -> Self {
        GradedAnswerG {
            question_id: gql_id(a.question_id),
            selected_option_ids: a.selected.into_iter().map(gql_id).collect(),
            correct_option_ids: a.correct_ids.into_iter().map(gql_id).collect(),
            correct: a.correct,
            awarded: a.awarded,
            possible: a.possible,
        }
    }
}

#[derive(SimpleObject, Debug)]
#[graphql(name = "Submission")]
pub struct SubmissionG {
    pub attempt: AttemptG,
    pub score: f64,
    pub max_score: f64,
    pub correct_count: i64,
    pub total_count: i64,
    pub answers: Vec<GradedAnswerG>,
}

#[derive(SimpleObject, Clone, Debug)]
#[graphql(name = "LeaderboardEntry")]
pub struct LeaderboardEntryG {
    pub rank: i64,
    pub user: UserRefG,
    pub score: f64,
    pub max_score: f64,
    pub submitted_at: i64,
}

impl From<LeaderboardEntry> for LeaderboardEntryG {
    fn from(e: LeaderboardEntry) -> Self {
        LeaderboardEntryG {
            rank: e.rank,
            user: UserRefG {
                id: gql_id(e.user_id),
                username: e.username,
            },
            score: e.score,
            max_score: e.max_score,
            submitted_at: e.submitted_at,
        }
    }
}

/// Payload of the `leaderboardUpdated` subscription: the fresh top-N list.
#[derive(SimpleObject, Clone, Debug)]
#[graphql(name = "LeaderboardUpdate")]
pub struct LeaderboardUpdateG {
    pub quiz_id: ID,
    pub quiz_title: String,
    pub entries: Vec<LeaderboardEntryG>,
}

impl From<LeaderboardEvent> for LeaderboardUpdateG {
    fn from(e: LeaderboardEvent) -> Self {
        LeaderboardUpdateG {
            quiz_id: gql_id(e.quiz_id),
            quiz_title: e.quiz_title,
            entries: e.entries.into_iter().map(Into::into).collect(),
        }
    }
}

/// An attempt together with its graded answers (owner only).
#[derive(SimpleObject, Debug)]
#[graphql(name = "AttemptDetail")]
pub struct AttemptDetailG {
    pub attempt: AttemptG,
    pub answers: Vec<GradedAnswerG>,
}

// ------------------------------------------------------------------ inputs

#[derive(InputObject, Clone, Debug)]
#[graphql(name = "QuizInput")]
pub struct QuizInputG {
    pub title: String,
    #[graphql(default)]
    pub description: String,
    #[graphql(default = true)]
    pub is_public: bool,
    pub time_limit_sec: Option<i64>,
    pub max_attempts: Option<i64>,
}

impl From<QuizInputG> for crate::repo::quizzes::NewQuiz {
    fn from(i: QuizInputG) -> Self {
        crate::repo::quizzes::NewQuiz {
            title: i.title,
            description: i.description,
            is_public: i.is_public,
            time_limit_sec: i.time_limit_sec,
            max_attempts: i.max_attempts,
        }
    }
}

#[derive(InputObject, Clone, Debug)]
#[graphql(name = "OptionInput")]
pub struct OptionInputG {
    pub text: String,
    pub is_correct: bool,
}

#[derive(InputObject, Clone, Debug)]
#[graphql(name = "QuestionInput")]
pub struct QuestionInputG {
    pub kind: QuestionKindG,
    pub text: String,
    #[graphql(default = 1.0)]
    pub points: f64,
    pub explanation: Option<String>,
    pub options: Vec<OptionInputG>,
}

impl From<QuestionInputG> for crate::repo::quizzes::NewQuestion {
    fn from(i: QuestionInputG) -> Self {
        crate::repo::quizzes::NewQuestion {
            kind: i.kind.into(),
            text: i.text,
            points: i.points,
            explanation: i.explanation,
            options: i
                .options
                .into_iter()
                .map(|o| crate::repo::quizzes::NewOption {
                    text: o.text,
                    is_correct: o.is_correct,
                })
                .collect(),
        }
    }
}

#[derive(InputObject, Clone, Debug)]
#[graphql(name = "AnswerInput")]
pub struct AnswerInputG {
    pub question_id: ID,
    pub selected_option_ids: Vec<ID>,
}

// ------------------------------------------------- row -> GraphQL mappings

/// Build a QuizG from a row plus its creator and question count.
pub fn quiz_g(row: QuizRow, creator: UserRow, question_count: i64) -> QuizG {
    QuizG {
        id: gql_id(row.id),
        title: row.title,
        description: row.description,
        is_public: row.is_public,
        time_limit_sec: row.time_limit_sec,
        max_attempts: row.max_attempts,
        question_count,
        created_at: row.created_at,
        updated_at: row.updated_at,
        creator: UserRefG {
            id: gql_id(creator.id),
            username: creator.username,
        },
    }
}

pub fn question_g(q: QuestionRow, options: Vec<OptionRow>, reveal_correct: bool) -> QuestionG {
    QuestionG {
        id: gql_id(q.id),
        position: q.position,
        kind: q.kind.into(),
        text: q.text,
        points: q.points,
        explanation: q.explanation,
        options: options
            .into_iter()
            .map(|o| OptionG {
                id: gql_id(o.id),
                position: o.position,
                text: o.text,
                is_correct: reveal_correct.then_some(o.is_correct),
            })
            .collect(),
    }
}
