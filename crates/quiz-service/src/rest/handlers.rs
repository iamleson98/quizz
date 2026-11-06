//! The bank: `/v1/quizzes`.

use super::dto::*;
use super::extract::{ApiJson, ApiQuery, Auth};
use crate::error::{AppError, AppResult, ErrorBody};
use crate::repo::{games, quizzes};
use crate::state::AppState;
use crate::tx;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::sync::Arc;
use utoipa::IntoParams;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 100;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListParams {
    /// Page size, 1 to 100 (default 50).
    #[param(minimum = 1, maximum = 100, default = 50)]
    pub limit: Option<i64>,
    /// Quizzes to skip (default 0).
    #[param(minimum = 0, default = 0)]
    pub offset: Option<i64>,
}

fn quiz_id(raw: &str) -> AppResult<i64> {
    raw.parse().map_err(|_| AppError::NotFound("quiz".into()))
}

/// The weak validator of a quiz: it changes whenever anything the quiz
/// shows changes (an edit, keeping it, a game started on it).
fn etag(row: &crate::model::QuizRow) -> String {
    format!("W/\"{:x}-{:x}\"", row.updated_at, row.expires_at)
}

async fn load_quiz(state: &AppState, id: i64, owner: &str) -> AppResult<Quiz> {
    let row = quizzes::owned(&state.pool, id, owner).await?;
    let questions = quizzes::questions(&state.pool, id).await?;
    Ok(Quiz::new(&row, questions))
}

fn not_while_running(state: &AppState, id: i64) -> AppResult<()> {
    if state.rooms.running_quiz_ids().contains(&id) {
        return Err(AppError::Conflict("a game is running on this quiz".into()));
    }
    Ok(())
}

/// List the caller's quizzes, newest first.
#[utoipa::path(
    get, path = "/v1/quizzes", operation_id = "listQuizzes", tag = "quizzes",
    params(ListParams),
    responses(
        (status = 200, description = "A page of the bank", body = QuizList),
        (status = 400, description = "Bad parameters", body = ErrorBody),
        (status = 401, description = "No valid ticket", body = ErrorBody),
        (status = 403, description = "The ticket's role has no bank", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_quizzes(
    State(state): State<AppState>,
    Auth(ticket): Auth,
    ApiQuery(p): ApiQuery<ListParams>,
) -> AppResult<Json<QuizList>> {
    ticket.require_bank()?;
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = p.offset.unwrap_or(0).max(0);
    let (rows, total) = quizzes::page(&state.pool, &ticket.sub, limit, offset).await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let counts = quizzes::question_counts(&state.pool, &ids).await?;
    let items = rows
        .iter()
        .map(|r| QuizSummary::new(r, counts.get(&r.id).copied().unwrap_or(0)))
        .collect();
    Ok(Json(QuizList { items, total }))
}

/// Create a quiz with all its questions.
#[utoipa::path(
    post, path = "/v1/quizzes", operation_id = "createQuiz", tag = "quizzes",
    request_body = CreateQuizBody,
    responses(
        (status = 201, description = "The new quiz", body = Quiz),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "No valid ticket", body = ErrorBody),
        (status = 403, description = "The ticket's role has no bank", body = ErrorBody),
        (status = 409, description = "The bank is full (200 quizzes)", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
        (status = 422, description = "Invalid input; `fields` says where", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn create_quiz(
    State(state): State<AppState>,
    Auth(ticket): Auth,
    ApiJson(body): ApiJson<CreateQuizBody>,
) -> AppResult<(StatusCode, Json<Quiz>)> {
    ticket.require_bank()?;
    let input = Arc::new(new_quiz(body.title, body.description, body.questions)?);
    let (owner, keep, now) = (ticket.sub.clone(), body.kept, state.now_ms());
    let id = tx::write_tx(&state, |conn| {
        let (owner, input) = (owner.clone(), input.clone());
        Box::pin(async move { quizzes::create(conn, &owner, &input, keep, now).await })
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(load_quiz(&state, id, &ticket.sub).await?),
    ))
}

/// Read a quiz with its answer keys.
#[utoipa::path(
    get, path = "/v1/quizzes/{id}", operation_id = "getQuiz", tag = "quizzes",
    params(("id" = String, Path, description = "Quiz id")),
    responses(
        (status = 200, description = "The quiz; carries an ETag", body = Quiz,
            headers(("ETag" = String, description = "Weak validator; send it back in If-None-Match"))),
        (status = 304, description = "Unchanged since the ETag in If-None-Match"),
        (status = 401, description = "No valid ticket", body = ErrorBody),
        (status = 403, description = "The ticket's role has no bank", body = ErrorBody),
        (status = 404, description = "No such quiz of yours", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn get_quiz(
    State(state): State<AppState>,
    Auth(ticket): Auth,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> AppResult<Response> {
    ticket.require_bank()?;
    let id = quiz_id(&id)?;
    let row = quizzes::owned(&state.pool, id, &ticket.sub).await?;
    let tag = etag(&row);
    let mut out_headers = HeaderMap::new();
    out_headers.insert(ETAG, HeaderValue::from_str(&tag).expect("etag is ascii"));
    out_headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    let matches = headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == tag || t.trim() == "*"));
    if matches {
        return Ok((StatusCode::NOT_MODIFIED, out_headers).into_response());
    }
    let questions = quizzes::questions(&state.pool, id).await?;
    Ok((out_headers, Json(Quiz::new(&row, questions))).into_response())
}

/// Save the whole quiz: title, description and every question at once.
#[utoipa::path(
    put, path = "/v1/quizzes/{id}", operation_id = "updateQuiz", tag = "quizzes",
    params(("id" = String, Path, description = "Quiz id")),
    request_body = UpdateQuizBody,
    responses(
        (status = 200, description = "The saved quiz", body = Quiz),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "No valid ticket", body = ErrorBody),
        (status = 403, description = "The ticket's role has no bank", body = ErrorBody),
        (status = 404, description = "No such quiz of yours", body = ErrorBody),
        (status = 409, description = "A game is running on this quiz", body = ErrorBody),
        (status = 413, description = "Body too large", body = ErrorBody),
        (status = 422, description = "Invalid input; `fields` says where", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn update_quiz(
    State(state): State<AppState>,
    Auth(ticket): Auth,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<UpdateQuizBody>,
) -> AppResult<Json<Quiz>> {
    ticket.require_bank()?;
    let id = quiz_id(&id)?;
    let input = Arc::new(new_quiz(body.title, body.description, body.questions)?);
    not_while_running(&state, id)?;
    let (owner, now) = (ticket.sub.clone(), state.now_ms());
    tx::write_tx(&state, |conn| {
        let (owner, input) = (owner.clone(), input.clone());
        Box::pin(async move { quizzes::update(conn, id, &owner, &input, now).await })
    })
    .await?;
    Ok(Json(load_quiz(&state, id, &ticket.sub).await?))
}

/// Keep a quiz (90 days after its last use) or let it go (7 days).
#[utoipa::path(
    put, path = "/v1/quizzes/{id}/kept", operation_id = "keepQuiz", tag = "quizzes",
    params(("id" = String, Path, description = "Quiz id")),
    request_body = KeptBody,
    responses(
        (status = 200, description = "The quiz as listed", body = QuizSummary),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "No valid ticket", body = ErrorBody),
        (status = 403, description = "The ticket's role has no bank", body = ErrorBody),
        (status = 404, description = "No such quiz of yours", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn keep_quiz(
    State(state): State<AppState>,
    Auth(ticket): Auth,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<KeptBody>,
) -> AppResult<Json<QuizSummary>> {
    ticket.require_bank()?;
    let id = quiz_id(&id)?;
    let (owner, keep, now) = (ticket.sub.clone(), body.kept, state.now_ms());
    tx::write_tx(&state, |conn| {
        let owner = owner.clone();
        Box::pin(async move { quizzes::set_kept(conn, id, &owner, keep, now).await })
    })
    .await?;
    let row = quizzes::owned(&state.pool, id, &ticket.sub).await?;
    let counts = quizzes::question_counts(&state.pool, &[id]).await?;
    Ok(Json(QuizSummary::new(
        &row,
        counts.get(&id).copied().unwrap_or(0),
    )))
}

/// Delete a quiz and its games at once.
#[utoipa::path(
    delete, path = "/v1/quizzes/{id}", operation_id = "deleteQuiz", tag = "quizzes",
    params(("id" = String, Path, description = "Quiz id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid ticket", body = ErrorBody),
        (status = 403, description = "The ticket's role has no bank", body = ErrorBody),
        (status = 404, description = "No such quiz of yours", body = ErrorBody),
        (status = 409, description = "A game is running on this quiz", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn delete_quiz(
    State(state): State<AppState>,
    Auth(ticket): Auth,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    ticket.require_bank()?;
    let id = quiz_id(&id)?;
    not_while_running(&state, id)?;
    let owner = ticket.sub.clone();
    tx::write_tx(&state, |conn| {
        let owner = owner.clone();
        Box::pin(async move { quizzes::delete(conn, id, &owner).await })
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The results of a quiz's finished games, newest first.
#[utoipa::path(
    get, path = "/v1/quizzes/{id}/games", operation_id = "listQuizGames", tag = "quizzes",
    params(("id" = String, Path, description = "Quiz id")),
    responses(
        (status = 200, description = "Finished games with their scoreboards", body = Vec<GameResult>),
        (status = 401, description = "No valid ticket", body = ErrorBody),
        (status = 403, description = "The ticket's role has no bank", body = ErrorBody),
        (status = 404, description = "No such quiz of yours", body = ErrorBody),
    ),
    security(("bearer" = []))
)]
pub async fn list_quiz_games(
    State(state): State<AppState>,
    Auth(ticket): Auth,
    Path(id): Path<String>,
) -> AppResult<Json<Vec<GameResult>>> {
    ticket.require_bank()?;
    let id = quiz_id(&id)?;
    quizzes::owned(&state.pool, id, &ticket.sub).await?;
    let results = games::results(&state.pool, id).await?;
    Ok(Json(results.into_iter().map(GameResult::from).collect()))
}
