//! GraphQL Mutation root. Every write runs through the BEGIN CONCURRENT
//! transaction engine (`crate::tx::write_tx`) with automatic conflict retry.

use super::types::*;
use crate::auth::require_user;
use crate::error::{AppError, GqlError};
use crate::model::LeaderboardEvent;
use crate::repo;
use crate::state::AppState;
use crate::tx;
use async_graphql::*;
use std::collections::HashMap;

pub struct MutationRoot;

#[Object]
impl MutationRoot {
    /// Create an account and receive a JWT.
    async fn register(
        &self,
        ctx: &Context<'_>,
        username: String,
        password: String,
    ) -> Result<AuthPayloadG, GqlError> {
        if !(8..=128).contains(&password.len()) {
            return Err(AppError::BadRequest("password must be 8-128 characters".into()).into());
        }
        let state = ctx.data::<AppState>()?;
        let hash = crate::auth::hash_password(&password)?;
        let user = tx::write_tx(state, move |conn| {
            let hash = hash.clone();
            let username = username.clone();
            Box::pin(async move { repo::users::create(conn, &username, &hash).await })
        })
        .await?;
        let token = crate::auth::issue_token(&state.cfg, user.id, &user.username)?;
        Ok(AuthPayloadG {
            token,
            user: user.into(),
        })
    }

    /// Exchange credentials for a JWT.
    async fn login(
        &self,
        ctx: &Context<'_>,
        username: String,
        password: String,
    ) -> Result<AuthPayloadG, GqlError> {
        let state = ctx.data::<AppState>()?;
        let user = repo::users::by_username(&state.pool, username.trim())
            .await?
            .ok_or_else(|| AppError::Unauthorized("invalid credentials".into()))?;
        if !crate::auth::verify_password(&password, &user.password_hash) {
            return Err(AppError::Unauthorized("invalid credentials".into()).into());
        }
        let token = crate::auth::issue_token(&state.cfg, user.id, &user.username)?;
        Ok(AuthPayloadG {
            token,
            user: user.into(),
        })
    }

    /// Create a quiz.
    async fn create_quiz(&self, ctx: &Context<'_>, input: QuizInputG) -> Result<QuizG, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let creator = repo::users::by_id(&state.pool, user.user_id)
            .await?
            .ok_or_else(|| AppError::NotFound("user".into()))?;

        let quiz = tx::write_tx(state, move |conn| {
            let input = input.clone();
            Box::pin(
                async move { repo::quizzes::create_quiz(conn, user.user_id, input.into()).await },
            )
        })
        .await?;

        Ok(super::types::quiz_g(quiz, creator, 0))
    }

    /// Update a quiz (creator only).
    async fn update_quiz(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: QuizInputG,
    ) -> Result<QuizG, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let quiz_id = parse_id(&id)?;

        let quiz = tx::write_tx(state, move |conn| {
            let input = input.clone();
            Box::pin(async move {
                repo::quizzes::update_quiz(conn, quiz_id, user.user_id, input.into()).await
            })
        })
        .await?;

        let creator = repo::users::by_id(&state.pool, quiz.creator_id)
            .await?
            .ok_or_else(|| AppError::NotFound("creator".into()))?;
        let count = repo::quizzes::question_count(&state.pool, quiz.id).await?;
        Ok(super::types::quiz_g(quiz, creator, count))
    }

    /// Delete a quiz and all its questions/attempts (creator only).
    async fn delete_quiz(&self, ctx: &Context<'_>, id: ID) -> Result<bool, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let quiz_id = parse_id(&id)?;
        tx::write_tx(state, move |conn| {
            Box::pin(async move { repo::quizzes::delete_quiz(conn, quiz_id, user.user_id).await })
        })
        .await?;
        Ok(true)
    }

    /// Add a batch of questions to a quiz (creator only).
    async fn add_questions(
        &self,
        ctx: &Context<'_>,
        quiz_id: ID,
        input: Vec<QuestionInputG>,
    ) -> Result<Vec<QuestionG>, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let qid = parse_id(&quiz_id)?;

        let created = tx::write_tx(state, move |conn| {
            let input = input.clone();
            Box::pin(async move {
                let new: Vec<repo::quizzes::NewQuestion> =
                    input.into_iter().map(Into::into).collect();
                repo::quizzes::add_questions(conn, qid, user.user_id, &new).await
            })
        })
        .await?;

        // Load the freshly created options (creator path: reveal correctness).
        let qws = repo::quizzes::questions_with_options(&state.pool, qid).await?;
        let mut by_id: HashMap<i64, QuestionG> = qws
            .into_iter()
            .map(|qw| {
                (
                    qw.question.id,
                    super::types::question_g(qw.question, qw.options, true),
                )
            })
            .collect();
        Ok(created
            .into_iter()
            .filter_map(|q| by_id.remove(&q.id))
            .collect())
    }

    /// Replace a question definition (creator only).
    async fn update_question(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: QuestionInputG,
    ) -> Result<QuestionG, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let question_id = parse_id(&id)?;

        let updated = tx::write_tx(state, move |conn| {
            let input = input.clone();
            Box::pin(async move {
                repo::quizzes::update_question(conn, question_id, user.user_id, input.into()).await
            })
        })
        .await?;

        let options = repo::quizzes::options_for_question(&state.pool, updated.id).await?;
        Ok(super::types::question_g(updated, options, true))
    }

    /// Delete a question (creator only).
    async fn delete_question(&self, ctx: &Context<'_>, id: ID) -> Result<bool, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let question_id = parse_id(&id)?;
        tx::write_tx(state, move |conn| {
            Box::pin(async move {
                repo::quizzes::delete_question(conn, question_id, user.user_id).await
            })
        })
        .await?;
        Ok(true)
    }

    /// Start an attempt on a quiz.
    async fn start_attempt(&self, ctx: &Context<'_>, quiz_id: ID) -> Result<AttemptG, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;
        let qid = parse_id(&quiz_id)?;

        let attempt = tx::write_tx(state, move |conn| {
            Box::pin(async move { repo::attempts::start(conn, qid, user.user_id).await })
        })
        .await?;
        Ok(attempt.into())
    }

    /// Submit answers for an attempt: grades server-side inside one
    /// BEGIN CONCURRENT transaction, then publishes a leaderboard refresh.
    async fn submit_attempt(
        &self,
        ctx: &Context<'_>,
        attempt_id: ID,
        answers: Vec<AnswerInputG>,
    ) -> Result<SubmissionG, GqlError> {
        let user = require_user(ctx)?;
        let state = ctx.data::<AppState>()?;

        let mut answer_map: HashMap<i64, Vec<i64>> = HashMap::new();
        for a in answers {
            let qid = parse_id(&a.question_id)?;
            let mut sel = Vec::with_capacity(a.selected_option_ids.len());
            for oid in &a.selected_option_ids {
                sel.push(parse_id(oid)?);
            }
            answer_map.insert(qid, sel);
        }
        let attempt_id_num = parse_id(&attempt_id)?;

        let submission = tx::write_tx(state, move |conn| {
            let answers = answer_map.clone();
            Box::pin(async move {
                repo::attempts::submit(conn, attempt_id_num, user.user_id, &answers).await
            })
        })
        .await?;

        // Post-commit: publish a fresh leaderboard snapshot to subscribers.
        if let Some((quiz, entries)) = publish_snapshot(state, submission.attempt.quiz_id).await {
            state.publish_leaderboard(LeaderboardEvent {
                quiz_id: submission.attempt.quiz_id,
                quiz_title: quiz.title,
                entries,
            });
        }

        let correct_count = submission
            .graded
            .answers
            .iter()
            .filter(|a| a.correct)
            .count() as i64;
        Ok(SubmissionG {
            attempt: submission.attempt.into(),
            score: submission.graded.score,
            max_score: submission.graded.max_score,
            correct_count,
            total_count: submission.graded.answers.len() as i64,
            answers: submission
                .graded
                .answers
                .into_iter()
                .map(Into::into)
                .collect(),
        })
    }
}

/// Fetch the current top-10 for the leaderboard event.
async fn publish_snapshot(
    state: &AppState,
    quiz_id: i64,
) -> Option<(crate::model::QuizRow, Vec<crate::model::LeaderboardEntry>)> {
    let quiz = repo::quizzes::by_id(&state.pool, quiz_id).await.ok()??;
    let entries = repo::attempts::leaderboard(&state.pool, quiz_id, 10)
        .await
        .ok()?;
    Some((quiz, entries))
}
