//! Games that survive a restart.
//!
//! A room writes the state of its running game to the `running_games` table
//! (one row per room): at every phase change, and while the state keeps
//! changing within a phase at most every [`SAVE_EVERY`]. A [`Writer`] does the
//! writing off the room's task: it keeps only the latest order, so a slow
//! database never queues work or blocks the room. The row goes when the game
//! ends. At startup [`restore`] reads the rows and puts every game back in a
//! room before the service accepts a socket; a row it cannot read (another
//! [`VERSION`], damaged, too old) is deleted and its game closed.
//!
//! Every deadline in a snapshot is a wall-clock time (unix ms), so the time
//! the service was down counts: a question whose time ran out meanwhile closes
//! on the first timer. Players and the host are the ticket's `sub`, the same
//! after a restart, so they re-attach as themselves.

use super::quest::resume::QuestSnap;
use super::registry::Deps;
use super::room::resume::RaceSnap;
use super::store::GameStore;
use crate::error::AppResult;
use crate::repo::running::RunningRow;
use crate::retention::RUNNING_GAME_TTL_MS;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// The layout of the snapshots this build writes and reads. Raise it with any
/// change to them: a row of another version is refused.
pub const VERSION: u32 = 1;
/// After a planned stop, the time players get to reconnect to the game that was put back
/// (web clients retry every 8 to 12 s): the game's deadlines are pushed on by it, and a
/// player who has not come back keeps counting toward closing a question for that long.
pub const RECONNECT_ALLOWANCE: Duration = Duration::from_secs(20);
/// While a game's state keeps changing within a phase it is written at most
/// this often.
pub const SAVE_EVERY: Duration = Duration::from_secs(3);

/// How many times an order is tried, waiting 1 s, 2 s, 4 s ... (at most 10 s)
/// in between, unless a newer one supersedes it.
const WRITE_TRIES: u32 = 8;
const WRITE_MAX_WAIT: Duration = Duration::from_secs(10);

/// A running game as it is stored.
#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub quiz_id: i64,
    /// Set when the service stopped on purpose (unix ms): the time it was down is not taken
    /// from the players (see [`Saved::downtime`]).
    pub paused_at: Option<i64>,
    pub game: SavedGame,
}

#[derive(Serialize, Deserialize)]
pub enum SavedGame {
    Race(RaceSnap),
    Quest(QuestSnap),
}

impl SavedGame {
    pub fn id(&self) -> i64 {
        match self {
            SavedGame::Race(g) => g.id(),
            SavedGame::Quest(g) => g.id(),
        }
    }
}

impl Saved {
    pub fn host(&self) -> &str {
        match &self.game {
            SavedGame::Race(g) => g.host(),
            SavedGame::Quest(g) => g.host(),
        }
    }

    /// For a game that was paused by a planned stop: how long it stood still (ms), the time
    /// between the stop and `now_ms`. Its clocks run on from where they stopped, and its
    /// deadlines are also pushed by [`RECONNECT_ALLOWANCE`]. `None` for a game that was not
    /// paused (the process died): its deadlines are wall-clock and the time down counts.
    pub fn downtime(&self, now_ms: i64) -> Option<i64> {
        self.paused_at.map(|at| (now_ms - at).max(0))
    }

    /// The row to store for `room`.
    pub fn row(&self, room: &str, now_ms: i64) -> Result<RunningRow, serde_json::Error> {
        Ok(RunningRow {
            room: room.to_string(),
            game_id: self.game.id(),
            kind: matches!(self.game, SavedGame::Quest(_)) as i64,
            version: i64::from(VERSION),
            snapshot: serde_json::to_vec(self)?,
            updated_at: now_ms,
        })
    }

    /// The game a stored row holds, or why it cannot be put back.
    pub fn read(row: &RunningRow, now_ms: i64) -> Result<Saved, String> {
        if row.version != i64::from(VERSION) {
            return Err(format!(
                "snapshot version {} (this build reads {VERSION})",
                row.version
            ));
        }
        if row.updated_at <= now_ms - RUNNING_GAME_TTL_MS {
            return Err("snapshot older than a running game can be".into());
        }
        let saved: Saved = serde_json::from_slice(&row.snapshot)
            .map_err(|e| format!("snapshot unreadable: {e}"))?;
        match &saved.game {
            SavedGame::Race(g) => g.check(),
            SavedGame::Quest(g) => g.check(),
        }
        .map_err(|why| format!("snapshot cannot be played on: {why}"))?;
        Ok(saved)
    }
}

enum Order {
    Save(RunningRow),
    /// A room's row, if it is still that game's.
    Delete(String, i64),
}

type Latest = (u64, Option<Arc<Order>>);

/// Writes a room's snapshots off its task. Holds one order at most: a newer
/// one replaces an older one that was not written yet, so the database is
/// asked for the state as of the latest change and never for a backlog.
pub struct Writer {
    orders: watch::Sender<Latest>,
    done: watch::Receiver<u64>,
    seq: u64,
}

impl Writer {
    pub fn start(store: Arc<dyn GameStore>) -> Writer {
        let (orders, rx) = watch::channel((0, None));
        let (done_tx, done) = watch::channel(0);
        tokio::spawn(write_loop(store, rx, done_tx));
        Writer {
            orders,
            done,
            seq: 0,
        }
    }

    pub fn save(&mut self, row: RunningRow) {
        self.send(Order::Save(row));
    }

    pub fn delete(&mut self, room: String, game_id: i64) {
        self.send(Order::Delete(room, game_id));
    }

    fn send(&mut self, order: Order) {
        self.seq += 1;
        self.orders.send_replace((self.seq, Some(Arc::new(order))));
    }

    /// Resolves when the latest order was written (or given up on).
    pub async fn flush(&self) {
        let _ = self.done.clone().wait_for(|d| *d >= self.seq).await;
    }
}

async fn write_loop(
    store: Arc<dyn GameStore>,
    mut orders: watch::Receiver<Latest>,
    done: watch::Sender<u64>,
) {
    loop {
        let (seq, order) = orders.borrow_and_update().clone();
        let mut superseded = false;
        // once the room is gone no newer order can come
        let mut closed = false;
        if let Some(order) = order {
            let mut wait = Duration::from_secs(1);
            for attempt in 1..=WRITE_TRIES {
                let res = match &*order {
                    Order::Save(row) => store.save_running(row.clone()).await,
                    Order::Delete(room, game_id) => {
                        store.delete_running(room.clone(), *game_id).await
                    }
                };
                match res {
                    Ok(()) => {
                        metrics::counter!("quiz_resume_writes_total").increment(1);
                        break;
                    }
                    Err(e) => {
                        metrics::counter!("quiz_resume_write_failures_total").increment(1);
                        tracing::warn!(error = %e, attempt, "writing a running game failed");
                        if attempt == WRITE_TRIES {
                            tracing::error!(
                                "a running game could not be written: a restart now would lose it"
                            );
                            break;
                        }
                        if wait_for_newer(&mut orders, &mut closed, wait).await {
                            superseded = true;
                            break;
                        }
                        wait = (wait * 2).min(WRITE_MAX_WAIT);
                    }
                }
            }
        }
        let _ = done.send(seq);
        if !superseded && orders.changed().await.is_err() {
            return;
        }
    }
}

/// Wait `wait` before trying an order again; true when a newer order came meanwhile (this one
/// is then dropped). With the room gone (`closed`) nothing newer can come: it just waits.
async fn wait_for_newer(
    orders: &mut watch::Receiver<Latest>,
    closed: &mut bool,
    wait: Duration,
) -> bool {
    if !*closed {
        tokio::select! {
            _ = tokio::time::sleep(wait) => return false,
            changed = orders.changed() => {
                if changed.is_ok() {
                    return true;
                }
                *closed = true;
            }
        }
    }
    tokio::time::sleep(wait).await;
    false
}

/// What [`restore`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Restored {
    pub resumed: usize,
    pub refused: usize,
}

/// Put every stored running game back in a room. Call it once, at startup,
/// before sockets are accepted. A game that cannot be put back is dropped
/// (its row deleted, which lets the startup sweep close it in the results).
pub async fn restore(deps: &Deps) -> AppResult<Restored> {
    let rows = deps.store.load_running().await?;
    let now = (deps.now_ms)();
    let mut out = Restored::default();
    for row in rows {
        let resumed = Saved::read(&row, now).and_then(|saved| {
            deps.rooms
                .restore(deps, &row.room, saved)
                .map_err(|e| e.to_string())
        });
        match resumed {
            Ok(()) => {
                out.resumed += 1;
                tracing::info!(room = %row.room, game = row.game_id, "a running game was put back");
            }
            Err(why) => {
                out.refused += 1;
                metrics::counter!("quiz_resume_refused_total").increment(1);
                tracing::warn!(room = %row.room, game = row.game_id, %why, "a running game was dropped");
                if let Err(e) = deps
                    .store
                    .delete_running(row.room.clone(), row.game_id)
                    .await
                {
                    tracing::error!(error = %e, room = %row.room, "deleting a dropped running game failed");
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
