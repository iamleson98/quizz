//! The running games: one row per room (see `migrations/0002_running_games.sql`).

use crate::error::AppResult;
use sqlx::{FromRow, SqliteConnection, SqlitePool};

/// A running game as stored.
#[derive(Clone, Debug, FromRow)]
pub struct RunningRow {
    pub room: String,
    pub game_id: i64,
    /// 0 for a race, 1 for a quest.
    pub kind: i64,
    pub version: i64,
    pub snapshot: Vec<u8>,
    pub updated_at: i64,
}

/// Store the state of a room's game, replacing the one before it.
pub async fn save(conn: &mut SqliteConnection, row: &RunningRow) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO running_games (room, game_id, kind, version, snapshot, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(room) DO UPDATE SET
            game_id = excluded.game_id, kind = excluded.kind, version = excluded.version,
            snapshot = excluded.snapshot, updated_at = excluded.updated_at",
    )
    .bind(&row.room)
    .bind(row.game_id)
    .bind(row.kind)
    .bind(row.version)
    .bind(&row.snapshot)
    .bind(row.updated_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Forget a room's game, unless the row is already another game's.
pub async fn delete(conn: &mut SqliteConnection, room: &str, game_id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM running_games WHERE room = ?1 AND game_id = ?2")
        .bind(room)
        .bind(game_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Delete the rows of games that have ended (or are gone): a crash can come between the end
/// of a game and the delete of its row.
pub async fn drop_ended(conn: &mut SqliteConnection) -> AppResult<()> {
    sqlx::query(
        "DELETE FROM running_games WHERE game_id NOT IN (SELECT id FROM games WHERE ended_at IS NULL)",
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Every stored game.
pub async fn all(pool: &SqlitePool) -> AppResult<Vec<RunningRow>> {
    Ok(sqlx::query_as(
        "SELECT room, game_id, kind, version, snapshot, updated_at FROM running_games",
    )
    .fetch_all(pool)
    .await?)
}
