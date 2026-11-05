//! User repository.

use crate::error::{AppError, AppResult};
use crate::ids::{new_id, now_ms};
use crate::model::UserRow;
use sqlx::{Executor, Sqlite};

const USER_COLS: &str = "id, username, password_hash, created_at";

pub async fn create(
    conn: &mut sqlx::SqliteConnection,
    username: &str,
    password_hash: &str,
) -> AppResult<UserRow> {
    let username = username.trim();
    if !(3..=32).contains(&username.chars().count()) {
        return Err(AppError::BadRequest(
            "username must be 3-32 characters".into(),
        ));
    }
    if !username
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(AppError::BadRequest(
            "username may only contain letters, digits, '_', '-' and '.'".into(),
        ));
    }

    let row = sqlx::query_as::<_, UserRow>(&format!(
        "INSERT INTO users ({USER_COLS}) VALUES (?1, ?2, ?3, ?4) RETURNING {USER_COLS}"
    ))
    .bind(new_id())
    .bind(username)
    .bind(password_hash)
    .bind(now_ms())
    .fetch_one(&mut *conn)
    .await;

    match row {
        Ok(user) => Ok(user),
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            Err(AppError::Conflict("username is already taken".into()))
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn by_username<'e, E>(exec: E, username: &str) -> AppResult<Option<UserRow>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let user = sqlx::query_as::<_, UserRow>(&format!(
        "SELECT {USER_COLS} FROM users WHERE username = ?1"
    ))
    .bind(username)
    .fetch_optional(exec)
    .await?;
    Ok(user)
}

pub async fn by_id<'e, E>(exec: E, id: i64) -> AppResult<Option<UserRow>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite>,
{
    let user =
        sqlx::query_as::<_, UserRow>(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"))
            .bind(id)
            .fetch_optional(exec)
            .await?;
    Ok(user)
}

/// Fetch many users by id in one query.
pub async fn by_ids<'e, E>(
    exec: E,
    ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, UserRow>>
where
    E: 'e + Send + Executor<'e, Database = Sqlite> + Clone,
{
    let mut out = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    for chunk in ids.chunks(500) {
        let placeholders = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT {USER_COLS} FROM users WHERE id IN ({placeholders})");
        let mut q = sqlx::query_as::<_, UserRow>(&sql);
        for id in chunk {
            q = q.bind(id);
        }
        for user in q.fetch_all(exec.clone()).await? {
            out.insert(user.id, user);
        }
    }
    Ok(out)
}
