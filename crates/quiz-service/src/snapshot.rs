//! Consistent snapshots of the database for the off-site backup.
//!
//! Every interval, when something changed since the last one, the database
//! is copied with `VACUUM INTO` to a temporary file next to the target, made
//! durable, and renamed over `quiz.db` (an atomic replace: a reader of the
//! directory sees the old snapshot or the new one, never half of one).
//!
//! `quiz_snapshot_last_success_seconds` is the Unix time of the last
//! interval at which the snapshot was known to be current. It advances at
//! EVERY interval, also when nothing changed and no file was written, so a
//! quiet night does not look like a failing backup.

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// The file the sidecar copies away.
pub const SNAPSHOT_FILE: &str = "quiz.db";
const TEMP_FILE: &str = "quiz.db.tmp";

/// What one snapshot attempt did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A new snapshot was written.
    Written,
    /// Nothing changed since the last one.
    Unchanged,
}

/// Remembers which state of the database the last snapshot holds.
pub struct Snapshotter {
    dir: PathBuf,
    /// `commits` when the last snapshot was taken; `None` before the first.
    last_commits: Option<u64>,
}

impl Snapshotter {
    pub fn new(dir: PathBuf) -> Self {
        Snapshotter {
            dir,
            last_commits: None,
        }
    }

    /// One interval: snapshot when something changed, and in either case
    /// record that the snapshot is current.
    pub async fn run_once(&mut self, state: &AppState) -> AppResult<Outcome> {
        let commits = state.commits.load(Ordering::Relaxed);
        let outcome = if self.last_commits == Some(commits) {
            Outcome::Unchanged
        } else {
            write_snapshot(state, &self.dir).await?;
            self.last_commits = Some(commits);
            Outcome::Written
        };
        metrics::gauge!("quiz_snapshot_last_success_seconds").set((state.now_ms() / 1000) as f64);
        Ok(outcome)
    }
}

async fn write_snapshot(state: &AppState, dir: &Path) -> AppResult<()> {
    let io = |e: std::io::Error| AppError::Internal(format!("snapshot: {e}"));
    tokio::fs::create_dir_all(dir).await.map_err(io)?;
    let tmp = dir.join(TEMP_FILE);
    let target = dir.join(SNAPSHOT_FILE);
    // VACUUM INTO refuses to overwrite: clear what a crash may have left.
    match tokio::fs::remove_file(&tmp).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let path = tmp
        .to_str()
        .ok_or_else(|| AppError::Internal("snapshot path is not UTF-8".into()))?;
    sqlx::query("VACUUM INTO ?1")
        .bind(path)
        .execute(&state.pool)
        .await?;
    // VACUUM INTO does not promise the bytes are on disk: make them so
    // before the rename publishes the file.
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .await
        .map_err(io)?;
    file.sync_all().await.map_err(io)?;
    drop(file);
    tokio::fs::rename(&tmp, &target).await.map_err(io)?;
    if let Ok(d) = tokio::fs::File::open(dir).await {
        let _ = d.sync_all().await;
    }
    Ok(())
}

/// Snapshot for the life of the process; does nothing when the directory
/// setting is empty.
pub fn spawn(state: AppState) {
    let Some(dir) = state.cfg.snapshot_dir.clone() else {
        tracing::info!("snapshots are off (QUIZ_SNAPSHOT_DIR is empty)");
        return;
    };
    tokio::spawn(async move {
        let mut snap = Snapshotter::new(dir);
        let mut tick = tokio::time::interval(state.cfg.snapshot_interval);
        loop {
            tick.tick().await;
            match snap.run_once(&state).await {
                Ok(Outcome::Written) => tracing::info!("snapshot written"),
                Ok(Outcome::Unchanged) => tracing::debug!("snapshot is current"),
                Err(e) => tracing::error!(error = %e, "snapshot failed"),
            }
        }
    });
}
