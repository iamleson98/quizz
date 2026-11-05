//! GraphQL Query root.

use super::types::*;
use crate::auth::{AuthUser, require_user};
use crate::error::{AppError, GqlError};
use crate::graphql::types;
use crate::model::{QuestionWithOptions, UserRow};
use crate::repo;
use crate::state::AppState;
use async_graphql::*;

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// The authenticated user (from the `Authorization: Bearer` header).
    async fn me(&self, ctx: &Context<'_>) -> Result<UserG, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let row = repo::users::by_id(&state.pool, user.user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("user".into()))?;
        Ok(row.into())
    }

    /// A quiz with its questions. Correct answers are only visible to the
    /// creator; private quizzes are only visible to their creator.
    async fn quiz(&self, ctx: &Context<'_>, id: ID) -> Result<QuizDetailG, GqlError> {
        let state = ctx.data::<AppState>()?;
        let viewer = ctx.data_opt::<AuthUser>();
        let quiz_id = parse_id(&id)?;

        let quiz =
            repo::quizzes::visible_quiz(&state.pool, quiz_id, viewer.map(|u| u.user_id)).await?;
        let creator = repo::users::by_id(&state.pool, quiz.creator_id)
            .await?
            .ok_or_else(|| AppError::NotFound("quiz creator".into()))?;
        let count = repo::quizzes::question_count(&state.pool, quiz.id).await?;
        let quiz_id = quiz.id;
        let is_creator = viewer.map(|u| u.user_id) == Some(quiz.creator_id);

        let questions = load_questions(state, quiz_id, is_creator).await?;
        Ok(QuizDetailG {
            quiz: types::quiz_g(quiz, creator, count),
            questions,
        })
    }

    /// Public quizzes (plus the viewer's own), newest first.
    async fn quizzes(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 20, validator(minimum = 1, maximum = 100))] first: i64,
        #[graphql(default = 0, validator(minimum = 0))] offset: i64,
    ) -> Result<QuizPageG, GqlError> {
        let state = ctx.data::<AppState>()?;
        let viewer = ctx.data_opt::<AuthUser>().map(|u| u.user_id);
        let (rows, total) = repo::quizzes::list_page(&state.pool, viewer, first, offset).await?;

        // One extra query for creator names + question counts (no N+1).
        let mut items = Vec::with_capacity(rows.len());
        if !rows.is_empty() {
            let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
            let counts = repo::quizzes::count_many(&state.pool, &ids).await?;
            let creators = repo::users::by_ids(&state.pool, &creator_ids(&rows)).await?;
            for row in rows {
                let creator = creators
                    .get(&row.creator_id)
                    .cloned()
                    .unwrap_or_else(|| UserRow {
                        id: row.creator_id,
                        username: "unknown".into(),
                        password_hash: String::new(),
                        created_at: 0,
                    });
                let count = counts.get(&row.id).copied().unwrap_or(0);
                items.push(types::quiz_g(row, creator, count));
            }
        }
        Ok(QuizPageG { items, total })
    }

    /// Top-N leaderboard of a quiz.
    async fn leaderboard(
        &self,
        ctx: &Context<'_>,
        quiz_id: ID,
        #[graphql(default = 10, validator(minimum = 1, maximum = 100))] first: i64,
    ) -> Result<Vec<LeaderboardEntryG>, GqlError> {
        let state = ctx.data::<AppState>()?;
        let qid = parse_id(&quiz_id)?;
        // Visibility check (public or viewer is owner).
        let viewer = ctx.data_opt::<AuthUser>().map(|u| u.user_id);
        repo::quizzes::visible_quiz(&state.pool, qid, viewer).await?;
        let entries = repo::attempts::leaderboard(&state.pool, qid, first).await?;
        Ok(entries.into_iter().map(Into::into).collect())
    }

    /// The authenticated user's attempts, optionally filtered by quiz.
    async fn my_attempts(
        &self,
        ctx: &Context<'_>,
        quiz_id: Option<ID>,
    ) -> Result<Vec<AttemptG>, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let qid = match quiz_id {
            Some(id) => Some(parse_id(&id)?),
            None => None,
        };
        let rows = repo::attempts::by_user(&state.pool, user.user_id, qid).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// One attempt with per-question grading (attempt owner only).
    async fn attempt(&self, ctx: &Context<'_>, id: ID) -> Result<AttemptDetailG, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let attempt_id = parse_id(&id)?;
        let (attempt, answers) =
            repo::attempts::with_answers(&state.pool, attempt_id, user.user_id).await?;
        let graded = answers
            .into_iter()
            .map(|a| {
                let selected: Vec<i64> = serde_json::from_str(&a.selected).unwrap_or_default();
                GradedAnswerG {
                    question_id: gql_id(a.question_id),
                    selected_option_ids: selected.into_iter().map(gql_id).collect(),
                    correct_option_ids: Vec::new(),
                    correct: a.is_correct,
                    awarded: a.awarded,
                    possible: a.possible,
                }
            })
            .collect();
        Ok(AttemptDetailG {
            attempt: attempt.into(),
            answers: graded,
        })
    }

    /// SQLite version string reported by the embedded custom build
    /// (the begin-concurrent branch this service links).
    async fn sqlite_info(&self, ctx: &Context<'_>) -> Result<String, GqlError> {
        let state = ctx.data::<AppState>()?;
        Ok(format!(
            "SQLite {} (begin-concurrent branch, tx mode: {})",
            state.sqlite_version,
            state.cfg.tx_mode.as_str()
        ))
    }
}

pub async fn load_questions(
    state: &AppState,
    quiz_id: i64,
    reveal_correct: bool,
) -> Result<Vec<QuestionG>, GqlError> {
    let qws: Vec<QuestionWithOptions> =
        repo::quizzes::questions_with_options(&state.pool, quiz_id).await?;
    Ok(qws
        .into_iter()
        .map(|qw| types::question_g(qw.question, qw.options, reveal_correct))
        .collect())
}

fn creator_ids(rows: &[crate::model::QuizRow]) -> Vec<i64> {
    let mut ids: Vec<i64> = rows.iter().map(|r| r.creator_id).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}
