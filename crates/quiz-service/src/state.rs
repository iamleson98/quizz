//! Shared application state.

use crate::clock::Clock;
use crate::config::Config;
use crate::game::store::DbStore;
use crate::game::{Deps, Rooms};
use jsonwebtoken::DecodingKey;
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

pub struct AppStateInner {
    pub cfg: Config,
    pub pool: SqlitePool,
    /// In-process COMMIT serializer for BEGIN CONCURRENT (see tx.rs).
    pub commit_mutex: Mutex<()>,
    /// Key tickets are verified with.
    pub ticket_key: DecodingKey,
    /// Rooms and running games.
    pub rooms: Rooms,
    /// What a room needs; set once the state exists.
    deps: OnceLock<Deps>,
    pub clock: Clock,
    /// Limits the bank's writes per person.
    pub write_limiter: crate::ratelimit::Limiter,
    /// SQLite version reported by our custom build (for /healthz).
    pub sqlite_version: String,
    pub started_at: std::time::Instant,
    /// Committed write transactions; the snapshot job compares it with the
    /// value at its last snapshot to know whether anything changed.
    pub commits: AtomicU64,
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
        let write_limiter =
            crate::ratelimit::Limiter::new(cfg.bank_write_burst, cfg.bank_writes_per_minute);
        let state = Self(Arc::new(AppStateInner {
            ticket_key: DecodingKey::from_secret(cfg.ticket_key.expose().as_bytes()),
            cfg,
            pool,
            commit_mutex: Mutex::new(()),
            rooms: Rooms::default(),
            deps: OnceLock::new(),
            clock: Clock::default(),
            write_limiter,
            sqlite_version,
            started_at: std::time::Instant::now(),
            commits: AtomicU64::new(0),
        }));
        let clock_state = state.clone();
        let _ = state.deps.set(Deps {
            store: Arc::new(DbStore {
                state: state.clone(),
            }),
            now_ms: Arc::new(move || clock_state.now_ms()),
            rooms: state.rooms.clone(),
            quest_draw: Arc::new(|| Box::new(crate::game::quest::chest::SystemDraw)),
        });
        Ok(state)
    }

    /// What rooms are built with.
    pub fn deps(&self) -> &Deps {
        self.deps
            .get()
            .expect("deps are set when the state is built")
    }

    /// Current unix time in milliseconds (the service clock).
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    pub fn commits(&self) -> u64 {
        self.commits.load(Ordering::Relaxed)
    }
}
