//! Shared application state.

use crate::config::Config;
use crate::model::LeaderboardEvent;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};

pub struct AppStateInner {
    pub cfg: Config,
    pub pool: SqlitePool,
    /// In-process COMMIT serializer for BEGIN CONCURRENT (see tx.rs).
    pub commit_mutex: Mutex<()>,
    /// Fan-out for leaderboard subscription events.
    pub leaderboard_tx: broadcast::Sender<LeaderboardEvent>,
    /// SQLite version reported by our custom build (for /healthz).
    pub sqlite_version: String,
    pub started_at: std::time::Instant,
}

#[derive(Clone)]
pub struct AppState(pub Arc<AppStateInner>);

impl std::ops::Deref for AppState {
    type Target = AppStateInner;
    fn deref(&self) -> &AppStateInner {
        &self.0
    }
}

impl AppState {
    /// Connect, migrate, and build full application state.
    pub async fn connect(cfg: Config) -> crate::error::AppResult<Self> {
        let (pool, sqlite_version) = crate::db::connect_pool(&cfg).await?;
        if cfg.seed_demo {
            crate::db::seed_demo(&pool).await?;
        }
        let (leaderboard_tx, _) = broadcast::channel(256);
        Ok(Self(Arc::new(AppStateInner {
            cfg,
            pool,
            commit_mutex: Mutex::new(()),
            leaderboard_tx,
            sqlite_version,
            started_at: std::time::Instant::now(),
        })))
    }

    /// Wrap an existing pool (used by seeding and tests).
    pub fn for_pool(pool: SqlitePool, cfg: Config) -> Self {
        let (leaderboard_tx, _) = broadcast::channel(256);
        Self(Arc::new(AppStateInner {
            cfg,
            pool,
            commit_mutex: Mutex::new(()),
            leaderboard_tx,
            sqlite_version: String::new(),
            started_at: std::time::Instant::now(),
        }))
    }

    /// Publish a leaderboard refresh event (no-op when nobody subscribes).
    pub fn publish_leaderboard(&self, event: LeaderboardEvent) {
        let _ = self.leaderboard_tx.send(event);
    }
}
