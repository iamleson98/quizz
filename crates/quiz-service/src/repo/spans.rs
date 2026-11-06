//! The spans outbox: connected stretches of running games waiting to be
//! pushed to the Go server.

use crate::error::AppResult;
use sqlx::{Executor, Sqlite, SqliteConnection};

/// A host's or a player's stretch of presence in a game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub game_id: i64,
    pub user_id: String,
    pub session_id: String,
    /// `host` or `player`.
    pub role: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub closed: bool,
}

/// Insert a span, or move an existing one's end later. A span's end only
/// grows and a closed span stays closed.
pub async fn upsert(conn: &mut SqliteConnection, s: &Span) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO spans_outbox (game_id, user_id, session_id, role, from_ms, to_ms, closed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(game_id, user_id, from_ms) DO UPDATE SET
            to_ms = MAX(to_ms, excluded.to_ms),
            closed = MAX(closed, excluded.closed)",
    )
    .bind(s.game_id)
    .bind(&s.user_id)
    .bind(&s.session_id)
    .bind(&s.role)
    .bind(s.from_ms)
    .bind(s.to_ms)
    .bind(s.closed)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

type Row = (i64, String, String, String, i64, i64, bool);

/// Up to `limit` spans whose end has not been acknowledged, oldest first.
pub async fn pending<'e, E>(exec: E, limit: i64) -> AppResult<Vec<Span>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT game_id, user_id, session_id, role, from_ms, to_ms, closed FROM spans_outbox
         WHERE to_ms > pushed_ms ORDER BY from_ms, game_id, user_id LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(game_id, user_id, session_id, role, from_ms, to_ms, closed)| Span {
                game_id,
                user_id,
                session_id,
                role,
                from_ms,
                to_ms,
                closed,
            },
        )
        .collect())
}

/// The server acknowledged `s` as sent: a closed span is done and leaves;
/// an open one remembers how far it was acknowledged.
pub async fn acknowledge(conn: &mut SqliteConnection, s: &Span) -> AppResult<()> {
    sqlx::query("UPDATE spans_outbox SET pushed_ms = MAX(pushed_ms, ?4) WHERE game_id = ?1 AND user_id = ?2 AND from_ms = ?3")
        .bind(s.game_id)
        .bind(&s.user_id)
        .bind(s.from_ms)
        .bind(s.to_ms)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "DELETE FROM spans_outbox
         WHERE game_id = ?1 AND user_id = ?2 AND from_ms = ?3 AND closed = 1 AND pushed_ms >= to_ms",
    )
    .bind(s.game_id)
    .bind(&s.user_id)
    .bind(s.from_ms)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Give up on a span the server will never accept.
pub async fn discard(conn: &mut SqliteConnection, s: &Span) -> AppResult<()> {
    sqlx::query("DELETE FROM spans_outbox WHERE game_id = ?1 AND user_id = ?2 AND from_ms = ?3")
        .bind(s.game_id)
        .bind(&s.user_id)
        .bind(s.from_ms)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Rows still in the outbox.
pub async fn depth<'e, E>(exec: E) -> AppResult<i64>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM spans_outbox")
        .fetch_one(exec)
        .await?)
}
