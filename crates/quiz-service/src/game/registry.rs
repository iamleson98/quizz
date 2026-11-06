//! The rooms of the running service: which session has a room, which games
//! are running, who is connected.

use super::frames::Out;
use super::room::{self, Member, RoomGone, RoomHandle};
use super::store::GameStore;
use crate::error::{AppError, AppResult};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// Sockets one person may hold at once: a fifth evicts the oldest.
pub const MAX_SOCKETS_PER_USER: usize = 4;
/// Sockets that have not said hello yet, all together (the backstop).
pub const MAX_UNKNOWN_SOCKETS: usize = 2000;
/// Sockets from one client address that have not said hello yet. Generous,
/// because a school is one address (NAT) with a class behind it.
pub const MAX_UNKNOWN_PER_IP: u32 = 64;

/// What a room needs from the service around it.
#[derive(Clone)]
pub struct Deps {
    pub store: Arc<dyn GameStore>,
    /// The service clock, unix ms.
    pub now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub rooms: Rooms,
    /// A fresh random source for each quest game: a CSPRNG in the service, a
    /// seeded one in tests.
    pub quest_draw: Arc<dyn Fn() -> Box<dyn super::quest::chest::Draw> + Send + Sync>,
}

#[derive(Default)]
struct Registry {
    rooms: HashMap<String, RoomHandle>,
    /// Sessions with a running game: host and quiz.
    running: HashMap<String, (String, i64)>,
    /// A person's sockets, oldest first: `(socket, how to tell it to go)`.
    conns: HashMap<String, VecDeque<(u64, Arc<Notify>)>>,
    /// Sockets that have not said hello yet, by client address.
    unknown_by_ip: HashMap<IpAddr, u32>,
}

#[derive(Default)]
struct Inner {
    reg: Mutex<Registry>,
    next_room: AtomicU64,
    next_sock: AtomicU64,
    unknown: std::sync::atomic::AtomicUsize,
}

/// All rooms. Cheap to clone.
#[derive(Clone, Default)]
pub struct Rooms(Arc<Inner>);

/// Counts one socket against its person's limit until dropped.
pub struct ConnGuard {
    rooms: Rooms,
    user: String,
    sock: u64,
    evicted: Arc<Notify>,
}

impl ConnGuard {
    /// Resolves when a newer socket of the same person pushed this one out.
    pub async fn evicted(&self) {
        self.evicted.notified().await;
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut reg = self.rooms.0.reg.lock().expect("rooms lock");
        if let Some(list) = reg.conns.get_mut(&self.user) {
            list.retain(|(sock, _)| *sock != self.sock);
            if list.is_empty() {
                reg.conns.remove(&self.user);
            }
        }
    }
}

/// Counts a socket that has not said hello yet until dropped.
pub struct UnknownGuard {
    rooms: Rooms,
    ip: IpAddr,
}

impl Drop for UnknownGuard {
    fn drop(&mut self) {
        self.rooms.0.unknown.fetch_sub(1, Ordering::Relaxed);
        let mut reg = self.rooms.0.reg.lock().expect("rooms lock");
        if let Some(n) = reg.unknown_by_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                reg.unknown_by_ip.remove(&self.ip);
            }
        }
    }
}

/// Why a socket that has yet to say hello was turned away.
#[derive(Debug, PartialEq, Eq)]
pub enum Turned {
    /// Too many from this client address: it cannot lock everybody else out.
    TooManyFromThisAddress,
    /// Too many in all.
    TooManyInAll,
}

impl Rooms {
    pub fn next_sock_id(&self) -> u64 {
        self.0.next_sock.fetch_add(1, Ordering::Relaxed)
    }

    /// Admit a socket that has yet to say hello from `ip`, or turn it away
    /// when that address already has [`MAX_UNKNOWN_PER_IP`] waiting or
    /// [`MAX_UNKNOWN_SOCKETS`] are waiting in all.
    pub fn admit_unknown(&self, ip: IpAddr) -> Result<UnknownGuard, Turned> {
        let mut reg = self.0.reg.lock().expect("rooms lock");
        if self.0.unknown.load(Ordering::Relaxed) >= MAX_UNKNOWN_SOCKETS {
            return Err(Turned::TooManyInAll);
        }
        let n = reg.unknown_by_ip.entry(ip).or_insert(0);
        if *n >= MAX_UNKNOWN_PER_IP {
            return Err(Turned::TooManyFromThisAddress);
        }
        *n += 1;
        self.0.unknown.fetch_add(1, Ordering::Relaxed);
        Ok(UnknownGuard {
            rooms: self.clone(),
            ip,
        })
    }

    /// Count socket `sock` of `user`. A person holds at most
    /// [`MAX_SOCKETS_PER_USER`]: the oldest is told to go when a newer one
    /// arrives, so a phone that lost its network never locks the person out.
    pub fn conn(&self, user: &str, sock: u64) -> ConnGuard {
        let mut reg = self.0.reg.lock().expect("rooms lock");
        let list = reg.conns.entry(user.to_string()).or_default();
        while list.len() >= MAX_SOCKETS_PER_USER {
            if let Some((_, evict)) = list.pop_front() {
                evict.notify_one();
            }
        }
        let evicted = Arc::new(Notify::new());
        list.push_back((sock, evicted.clone()));
        ConnGuard {
            rooms: self.clone(),
            user: user.to_string(),
            sock,
            evicted,
        }
    }

    /// Attach a socket to the room of `sid`, creating the room when needed.
    pub async fn attach(&self, deps: &Deps, sid: &str, member: Member) -> AppResult<RoomHandle> {
        for _ in 0..3 {
            let handle = {
                let mut reg = self.0.reg.lock().expect("rooms lock");
                reg.rooms
                    .entry(sid.to_string())
                    .or_insert_with(|| {
                        let id = self.0.next_room.fetch_add(1, Ordering::Relaxed);
                        room::spawn(deps.clone(), sid.to_string(), id)
                    })
                    .clone()
            };
            match handle.attach(member.clone()).await {
                Ok(res) => return res.map(|()| handle),
                Err(RoomGone) => self.room_closed(sid, handle.id),
            }
        }
        Err(AppError::Internal("room kept closing".into()))
    }

    /// A room's task ended.
    pub fn room_closed(&self, sid: &str, id: u64) {
        let mut reg = self.0.reg.lock().expect("rooms lock");
        if reg.rooms.get(sid).is_some_and(|h| h.id == id) {
            reg.rooms.remove(sid);
        }
    }

    /// A host is about to open a game: one running game per session and
    /// per teacher.
    pub fn try_begin(&self, sid: &str, host: &str, quiz_id: i64) -> AppResult<()> {
        let mut reg = self.0.reg.lock().expect("rooms lock");
        if reg.running.contains_key(sid) {
            return Err(AppError::Conflict(
                "a game is already running in this session".into(),
            ));
        }
        if reg.running.values().any(|(h, _)| h == host) {
            return Err(AppError::Conflict("you already have a running game".into()));
        }
        reg.running
            .insert(sid.to_string(), (host.to_string(), quiz_id));
        metrics::gauge!("quiz_games_running").increment(1.0);
        Ok(())
    }

    /// The game of `sid` is over.
    pub fn game_ended(&self, sid: &str) {
        let mut reg = self.0.reg.lock().expect("rooms lock");
        if reg.running.remove(sid).is_some() {
            metrics::gauge!("quiz_games_running").decrement(1.0);
        }
    }

    /// Quizzes that have a game running (they must not be edited or swept).
    pub fn running_quiz_ids(&self) -> HashSet<i64> {
        let reg = self.0.reg.lock().expect("rooms lock");
        reg.running.values().map(|(_, q)| *q).collect()
    }

    fn handles(&self) -> Vec<RoomHandle> {
        self.0
            .reg
            .lock()
            .expect("rooms lock")
            .rooms
            .values()
            .cloned()
            .collect()
    }

    /// Forget a person in every running game (they are dropped from the
    /// rooms; a game they host ends).
    pub async fn forget(&self, user: &str) {
        for h in self.handles() {
            h.forget(user.to_string()).await;
        }
    }

    /// End every game and close every socket, waiting at most `limit`.
    pub async fn shutdown(&self, limit: Duration) {
        let all = futures::future::join_all(self.handles().into_iter().map(|h| async move {
            h.shutdown().await;
        }));
        let _ = tokio::time::timeout(limit, all).await;
    }
}

/// The queue between a room and one socket's task.
pub fn out_channel() -> (
    tokio::sync::mpsc::Sender<Out>,
    tokio::sync::mpsc::Receiver<Out>,
) {
    tokio::sync::mpsc::channel(room::OUT_QUEUE)
}
