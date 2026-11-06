//! A live session's room: one task that owns every socket attached to the
//! session and the game being played in it.
//!
//! Sockets attach before a game exists (the students are already in the
//! room), a host opens a lobby for a quiz, and the game runs here until it
//! ends; the room then waits for the next one. Everything arrives through one
//! queue and every change goes out as shared bytes: nothing is sent to anyone
//! per answer except the answerer's own tiny acknowledgement.
//!
//! All timing goes through tokio time, so tests run with a paused clock.

use super::frames::{self, ClientFrame, EndReason, Out, Tail};
use super::registry::Deps;
use super::scoring::{points_for, streak_bonus};
use crate::auth::Role;
use crate::error::{AppError, AppResult};
use crate::grading::grade_question;
use crate::ids::new_id;
use crate::model::{QuestionKind, QuestionWithOptions};
use crate::repo::games::{AnswerWrite, Batch, NewGame, PlayerWrite};
use crate::repo::quizzes::limits::MAX_PLAYERS_PER_GAME;
use axum::extract::ws::Utf8Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// The host's `progress` count is refreshed at most this often.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(500);
/// A game with no command for this long ends by itself.
pub const IDLE_LIMIT: Duration = Duration::from_secs(60 * 60);
/// A socket stays open this long after its ticket's `rc` (the room's close
/// time): the game has ended at `rc`, and the people still there see it end.
/// Then every socket is closed (1001 "room closed"), and a hello whose `rc`
/// is further past than this is refused.
pub const ROOM_CLOSE_GRACE: Duration = Duration::from_secs(20);
/// A running game whose host has had no socket for this long ends
/// ([`EndReason::HostLeft`]); a host back sooner (any tab) cancels it.
pub const HOST_AWAY_LIMIT: Duration = Duration::from_secs(120);
/// A running game with no socket at all for this long ends
/// ([`EndReason::Idle`]), so the room is freed.
pub const EMPTY_ROOM_LIMIT: Duration = Duration::from_secs(5 * 60);
/// Frames a socket may have waiting before it is cut as too slow.
pub const OUT_QUEUE: usize = 8;
/// The beat between a question closing and its reveal (the correct options
/// and the ranking), in milliseconds. The host's `next` skips it.
pub const REVEAL_DELAY_MS: u64 = 2500;
/// Rows of the host's provisional ranking.
pub const PROVISIONAL_TOP: usize = 10;
/// How many commands may wait for the room.
const CMD_QUEUE: usize = 1024;

/// Who is attaching a socket.
#[derive(Clone)]
pub struct Member {
    pub sock: u64,
    pub user: String,
    pub name: String,
    pub role: Role,
    pub cid: String,
    /// Room close time from the ticket, unix ms.
    pub rc: i64,
    pub out: mpsc::Sender<Out>,
}

pub enum Cmd {
    Attach(Member, oneshot::Sender<AppResult<()>>),
    Detach(u64),
    /// A client frame and the instant it was received by the socket (not when
    /// the room got to it): answers are timed from it.
    Frame(u64, ClientFrame, Instant),
    Forget(String, oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
}

/// The way into a room.
#[derive(Clone)]
pub struct RoomHandle {
    pub id: u64,
    tx: mpsc::Sender<Cmd>,
}

/// The room task has ended: ask the registry for a fresh one.
#[derive(Debug)]
pub struct RoomGone;

impl RoomHandle {
    pub async fn attach(&self, member: Member) -> Result<AppResult<()>, RoomGone> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Attach(member, tx))
            .await
            .map_err(|_| RoomGone)?;
        rx.await.map_err(|_| RoomGone)
    }

    pub async fn detach(&self, sock: u64) {
        let _ = self.tx.send(Cmd::Detach(sock)).await;
    }

    pub async fn frame(&self, sock: u64, frame: ClientFrame, received: Instant) {
        let _ = self.tx.send(Cmd::Frame(sock, frame, received)).await;
    }

    pub async fn forget(&self, user: String) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Forget(user, tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

pub(super) struct Sock {
    pub(super) user: String,
    pub(super) name: String,
    pub(super) role: Role,
    pub(super) cid: String,
    pub(super) rc: i64,
    /// When the socket is closed: `rc` plus [`ROOM_CLOSE_GRACE`].
    pub(super) close_at: Instant,
    pub(super) out: mpsc::Sender<Out>,
    /// Index into the game's players, for a player socket in a game.
    pub(super) pid: Option<usize>,
}

impl Drop for Sock {
    fn drop(&mut self) {
        if self.role == Role::Player {
            metrics::gauge!("quiz_players_connected").decrement(1.0);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Lobby,
    Question,
    /// The question is closed; the reveal comes at `reveal_at`.
    Closing,
    Reveal,
    Podium,
}

struct Answer {
    question_id: i64,
    selected: Vec<i64>,
    mask: u32,
    correct: bool,
    credit: f64,
    /// Points for this answer, the streak bonus included once the question
    /// has closed.
    points: i64,
    /// The streak bonus part of `points`.
    bonus: i64,
    time_ms: i64,
}

struct Player {
    id: i64,
    /// `None` once the person is forgotten.
    user: Option<String>,
    name: String,
    joined_at: i64,
    /// First question this player can answer (0-based).
    first_question: usize,
    score: i64,
    total_ms: i64,
    correct_count: i64,
    /// Fully right answers in a row, as of the last reveal.
    streak: u32,
    /// The longest streak this game.
    best_streak: u32,
    /// Questions answered.
    answered: u32,
    /// Sockets of this player attached now.
    live: u32,
    /// In the roster (a lobby player who left is not).
    active: bool,
    dirty: bool,
    /// 1-based rank at the last reveal; 0 until ranked.
    rank: u32,
    answer: Option<Answer>,
}

struct Game {
    id: i64,
    title: String,
    host: String,
    questions: Arc<Vec<QuestionWithOptions>>,
    phase: Phase,
    /// Current question (0-based), meaningful in Question and Reveal.
    qi: usize,
    q_started: Instant,
    q_deadline: Instant,
    /// When a closed question is revealed (Closing only).
    reveal_at: Instant,
    players: Vec<Player>,
    by_user: HashMap<String, usize>,
    active_count: usize,
    answered: usize,
    /// How many players have chosen each option of the open question.
    choices: Vec<u32>,
    /// Names of players who joined since the last names frame to the host.
    new_names: Vec<String>,
    /// Player indexes best first, as of the last reveal.
    standings: Vec<usize>,
    pending: Vec<AnswerWrite>,
    rc_at: Instant,
    idle_at: Instant,
    /// The reveal frame without its closing bracket, once revealed.
    reveal: Option<Utf8Bytes>,
    /// The results were written (with the end of the game) when the podium
    /// opened: the end of the game does not write them again.
    stored: bool,
    /// The podium frame for players (open) and for the host (complete).
    podium: Option<(Utf8Bytes, Utf8Bytes)>,
}

impl Game {
    /// Active players that are connected, can answer this question, and
    /// those among them who have.
    fn answer_progress(&self) -> (usize, usize) {
        let (mut eligible, mut answered) = (0, 0);
        for p in &self.players {
            if p.active && p.live > 0 && p.first_question <= self.qi {
                eligible += 1;
                answered += p.answer.is_some() as usize;
            }
        }
        (eligible, answered)
    }

    /// The provisional top ten while a question is open: every player's
    /// total before it plus what their answer has earned so far, ties broken
    /// by total answer time so far, then by who joined first. These are the
    /// very points the close will apply.
    fn provisional_top(&self) -> Vec<(&str, i64)> {
        let total = |p: &Player| p.score + p.answer.as_ref().map_or(0, |a| a.points);
        let time = |p: &Player| p.total_ms + p.answer.as_ref().map_or(0, |a| a.time_ms);
        let mut order: Vec<usize> = (0..self.players.len())
            .filter(|&i| self.players[i].active)
            .collect();
        order.sort_by(|&a, &b| {
            let (pa, pb) = (&self.players[a], &self.players[b]);
            total(pb)
                .cmp(&total(pa))
                .then(time(pa).cmp(&time(pb)))
                .then(pa.joined_at.cmp(&pb.joined_at))
                .then(a.cmp(&b))
        });
        order
            .into_iter()
            .take(PROVISIONAL_TOP)
            .map(|i| (self.players[i].name.as_str(), total(&self.players[i])))
            .collect()
    }

    fn rows(&self, take: usize) -> Vec<(&str, i64)> {
        self.standings
            .iter()
            .take(take)
            .map(|&i| (self.players[i].name.as_str(), self.players[i].score))
            .collect()
    }
}

/// Run a room until it has no sockets and no game left.
pub struct Room {
    pub(super) sid: String,
    pub(super) id: u64,
    pub(super) deps: Deps,
    pub(super) socks: HashMap<u64, Sock>,
    game: Option<Game>,
    /// The quest game, when this room plays one instead of a race.
    pub(super) quest: Option<super::quest::Quest>,
    pub(super) wall0: i64,
    pub(super) t0: Instant,
    /// Sockets to drop after the current command (too slow or gone).
    pub(super) slow: Vec<u64>,
    /// The earliest `close_at` of the sockets (it may be early after a socket
    /// left; the timer recomputes it).
    sock_close_at: Option<Instant>,
    /// When the game's host has had no socket for [`HOST_AWAY_LIMIT`].
    host_gone_at: Option<Instant>,
    /// When the game has had no socket at all for [`EMPTY_ROOM_LIMIT`].
    empty_at: Option<Instant>,
    pub(super) progress_dirty: bool,
    pub(super) count_dirty: bool,
    pub(super) flush_at: Option<Instant>,
    pub(super) last_flush: Instant,
}

/// Start the room task for session `sid`.
pub fn spawn(deps: Deps, sid: String, id: u64) -> RoomHandle {
    let (tx, rx) = mpsc::channel(CMD_QUEUE);
    let now = Instant::now();
    let room = Room {
        sid,
        id,
        wall0: (deps.now_ms)(),
        deps,
        socks: HashMap::new(),
        game: None,
        quest: None,
        t0: now,
        slow: Vec::new(),
        sock_close_at: None,
        host_gone_at: None,
        empty_at: None,
        progress_dirty: false,
        count_dirty: false,
        flush_at: None,
        last_flush: now - PROGRESS_EVERY,
    };
    tokio::spawn(room.run(rx));
    RoomHandle { id, tx }
}

async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

impl Room {
    pub(super) fn wall(&self, at: Instant) -> i64 {
        self.wall0 + at.saturating_duration_since(self.t0).as_millis() as i64
    }

    pub(super) fn now_wall(&self) -> i64 {
        self.wall(Instant::now())
    }

    /// The instant a socket with room-close time `rc` (unix ms) is closed.
    pub(super) fn close_at(&self, rc: i64, now: Instant) -> Instant {
        let until_rc = Duration::from_millis((rc - self.wall(now)).max(0) as u64);
        now + until_rc + ROOM_CLOSE_GRACE
    }

    /// A socket with this `close_at` joined.
    pub(super) fn note_close_at(&mut self, at: Instant) {
        self.sock_close_at = Some(self.sock_close_at.map_or(at, |t| t.min(at)));
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Cmd>) {
        loop {
            let wake = self.next_wake();
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    if self.command(cmd).await {
                        break;
                    }
                }
                _ = sleep_until_opt(wake) => self.on_timer().await,
            }
            self.reap().await;
            self.watch_presence();
            if self.socks.is_empty() && self.game.is_none() && self.quest.is_none() {
                break;
            }
        }
        // Tell the registry first, so a socket arriving now gets a fresh
        // room, then fail what is still queued.
        self.deps.rooms.room_closed(&self.sid, self.id);
        rx.close();
    }

    /// The earliest moment something here needs attention.
    fn next_wake(&self) -> Option<Instant> {
        let g = self.game.as_ref();
        [
            g.filter(|g| g.phase == Phase::Question)
                .map(|g| g.q_deadline),
            g.filter(|g| g.phase == Phase::Closing).map(|g| g.reveal_at),
            g.map(|g| g.rc_at),
            g.map(|g| g.idle_at),
            self.flush_at,
            self.quest_next_wake(),
            self.sock_close_at,
            self.empty_at,
            // with nobody connected the empty-room limit decides
            self.host_gone_at.filter(|_| !self.socks.is_empty()),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        if self
            .game
            .as_ref()
            .is_some_and(|g| g.phase == Phase::Question && now >= g.q_deadline)
        {
            self.close_question().await;
        }
        if self
            .game
            .as_ref()
            .is_some_and(|g| g.phase == Phase::Closing && now >= g.reveal_at)
        {
            self.reveal_question().await;
        }
        if self.flush_at.is_some_and(|t| now >= t) {
            self.flush();
        }
        if let Some(g) = &self.game {
            if now >= g.rc_at {
                self.finish(EndReason::RoomClosed).await;
            } else if now >= g.idle_at {
                self.finish(EndReason::Idle).await;
            }
        }
        self.quest_on_timer().await;
        self.end_abandoned_game(now).await;
        self.close_expired_sockets(now).await;
    }

    /// Keep the two abandonment timers right: they run only while a game
    /// does, the host's from the moment the last host socket went, the
    /// room's from the moment the last socket of anybody went.
    fn watch_presence(&mut self) {
        if self.game.is_none() && self.quest.is_none() {
            self.host_gone_at = None;
            self.empty_at = None;
            return;
        }
        let now = Instant::now();
        let host_here = self.socks.values().any(|s| s.role == Role::Host);
        self.host_gone_at = if host_here {
            None
        } else {
            self.host_gone_at.or(Some(now + HOST_AWAY_LIMIT))
        };
        self.empty_at = if self.socks.is_empty() {
            self.empty_at.or(Some(now + EMPTY_ROOM_LIMIT))
        } else {
            None
        };
    }

    /// End a game nobody is running: the host stayed away, or everybody did.
    async fn end_abandoned_game(&mut self, now: Instant) {
        if self.game.is_none() && self.quest.is_none() {
            return;
        }
        if !self.socks.is_empty() && self.host_gone_at.is_some_and(|t| now >= t) {
            self.end_host_left().await;
        } else if self.empty_at.is_some_and(|t| now >= t) {
            self.finish(EndReason::Idle).await;
        }
    }

    /// Close the sockets whose ticket's room closed a grace ago. By then the
    /// game has ended (at `rc`) and its frames are queued ahead of the close.
    async fn close_expired_sockets(&mut self, now: Instant) {
        if !self.sock_close_at.is_some_and(|t| now >= t) {
            return;
        }
        let due: Vec<u64> = self
            .socks
            .iter()
            .filter(|(_, s)| now >= s.close_at)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            if let Some(s) = self.socks.get(&id) {
                let _ = s.out.try_send(Out::Close(1001, "room closed"));
            }
            self.detach(id).await;
        }
        self.sock_close_at = self.socks.values().map(|s| s.close_at).min();
    }

    /// Handle one command; true when the room must stop now.
    async fn command(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Attach(member, reply) => {
                let res = self.attach(member);
                let _ = reply.send(res);
            }
            Cmd::Detach(sock) => self.detach(sock).await,
            Cmd::Frame(sock, frame, at) => self.frame(sock, frame, at).await,
            Cmd::Forget(user, reply) => {
                self.forget(&user).await;
                let _ = reply.send(());
            }
            Cmd::Shutdown(reply) => {
                if self.game.is_some() || self.quest.is_some() {
                    self.finish(EndReason::Restart).await;
                }
                for s in self.socks.values() {
                    let _ = s.out.try_send(Out::Close(1001, "restart"));
                }
                self.socks.clear();
                let _ = reply.send(());
                return true;
            }
        }
        false
    }

    // ----------------------------------------------------------- sending

    pub(super) fn push(&mut self, sock: u64, out: Out) {
        if let Some(s) = self.socks.get(&sock)
            && s.out.try_send(out).is_err()
        {
            self.slow.push(sock);
        }
    }

    /// Send every socket what `each` makes for it (None: nothing).
    fn broadcast(&mut self, mut each: impl FnMut(&Sock, &Self) -> Option<Out>) {
        let mut slow = Vec::new();
        for (id, s) in &self.socks {
            if let Some(out) = each(s, self)
                && s.out.try_send(out).is_err()
            {
                slow.push(*id);
            }
        }
        self.slow.extend(slow);
    }

    pub(super) fn broadcast_shared(&mut self, frame: Utf8Bytes) {
        self.broadcast(|_, _| Some(Out::Plain(frame.clone())));
    }

    pub(super) fn broadcast_hosts(&mut self, frame: Utf8Bytes) {
        self.broadcast(|s, _| (s.role == Role::Host).then(|| Out::Plain(frame.clone())));
    }

    /// Detach sockets that cannot keep up or went away.
    async fn reap(&mut self) {
        while let Some(id) = self.slow.pop() {
            if let Some(s) = self.socks.get(&id) {
                let _ = s.out.try_send(Out::Close(1013, "slow"));
            }
            self.detach(id).await;
        }
    }

    // ------------------------------------------------------ attach/detach

    fn attach(&mut self, m: Member) -> AppResult<()> {
        if m.role == Role::Teacher {
            return Err(AppError::Forbidden);
        }
        if self.quest.is_some() {
            return self.quest_attach(m);
        }
        let now = Instant::now();
        let wall = self.wall(now);
        let close_at = self.close_at(m.rc, now);
        let mut pid = None;
        if let Some(g) = &mut self.game {
            match m.role {
                Role::Host if g.host != m.user => return Err(AppError::Forbidden),
                Role::Player => pid = Some(join(g, &m.user, &m.name, wall)?),
                _ => {}
            }
        }
        if m.role == Role::Player {
            metrics::gauge!("quiz_players_connected").increment(1.0);
        }
        self.socks.insert(
            m.sock,
            Sock {
                user: m.user,
                name: m.name,
                role: m.role,
                cid: m.cid,
                rc: m.rc,
                close_at,
                out: m.out,
                pid,
            },
        );
        self.note_close_at(close_at);
        let sync = self.sync_frame(m.sock);
        self.push(m.sock, sync);
        if m.role == Role::Host {
            self.send_host_catch_up(m.sock);
        }
        if m.role == Role::Player && self.game.is_some() {
            self.mark_count_dirty();
        }
        Ok(())
    }

    pub(super) async fn detach(&mut self, sock: u64) {
        if self.quest.is_some() {
            return self.quest_detach(sock).await;
        }
        let Some(s) = self.socks.remove(&sock) else {
            return;
        };
        let mut question_may_close = false;
        if let (Some(g), Some(pid)) = (&mut self.game, s.pid) {
            let p = &mut g.players[pid];
            p.live = p.live.saturating_sub(1);
            if p.live == 0 {
                if g.phase == Phase::Lobby && p.active {
                    p.active = false;
                    g.active_count -= 1;
                }
                question_may_close = g.phase == Phase::Question;
            }
        }
        if s.pid.is_some() {
            self.mark_count_dirty();
        }
        if question_may_close && self.all_answered() {
            self.close_question().await;
        }
    }

    fn all_answered(&self) -> bool {
        self.game.as_ref().is_some_and(|g| {
            let (eligible, answered) = g.answer_progress();
            eligible > 0 && eligible == answered
        })
    }

    async fn forget(&mut self, user: &str) {
        if self.quest.is_some() {
            return self.quest_forget(user).await;
        }
        if self.game.as_ref().is_some_and(|g| g.host == user) {
            self.finish(EndReason::Host).await;
        }
        let gone: Vec<u64> = self
            .socks
            .iter()
            .filter(|(_, s)| s.user == user)
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            if let Some(s) = self.socks.get(&id) {
                let _ = s.out.try_send(Out::Close(1008, "forgotten"));
            }
            self.detach(id).await;
        }
        if let Some(g) = &mut self.game
            && let Some(pid) = g.by_user.remove(user)
        {
            let p = &mut g.players[pid];
            p.user = None;
            p.name.clear();
            p.dirty = true;
        }
    }

    // ------------------------------------------------------------- frames

    async fn frame(&mut self, sock: u64, frame: ClientFrame, at: Instant) {
        let Some(s) = self.socks.get(&sock) else {
            return;
        };
        let (role, user) = (s.role, s.user.clone());
        let kind = match &frame {
            ClientFrame::Hello(_) => frames::client::HELLO,
            ClientFrame::Start(_) => frames::client::START,
            ClientFrame::Next => frames::client::NEXT,
            ClientFrame::End => frames::client::END,
            ClientFrame::Answer { .. } => frames::client::ANSWER,
            ClientFrame::StartQuest { .. } => frames::client::START,
            ClientFrame::Character(_) => frames::client::CHARACTER,
            ClientFrame::QuestAnswer { .. } => frames::client::QUEST_ANSWER,
            ClientFrame::QuestPick { .. } => frames::client::QUEST_PICK,
            ClientFrame::QuestNext => frames::client::QUEST_NEXT,
        };
        let res = match frame {
            ClientFrame::Hello(_) => Err(AppError::BadRequest("already said hello".into())),
            ClientFrame::Start(quiz_id) => self.start(sock, role, &user, quiz_id, None).await,
            ClientFrame::StartQuest {
                quiz_id,
                seconds,
                flags,
            } => {
                self.start(sock, role, &user, quiz_id, Some((seconds, flags)))
                    .await
            }
            // while a quest game exists, the host's next/end and the
            // players' frames are the quest's
            frame if self.quest.is_some() => self.quest_frame(sock, role, &user, frame, at).await,
            ClientFrame::Character(_)
            | ClientFrame::QuestAnswer { .. }
            | ClientFrame::QuestPick { .. }
            | ClientFrame::QuestNext => Err(AppError::Conflict("there is no quest game".into())),
            ClientFrame::Next => self.next(role, &user).await,
            ClientFrame::End => self.end(role, &user).await,
            ClientFrame::Answer { q, mask } => self.answer(sock, role, q, mask, at).await,
        };
        if let Err(e) = res {
            tracing::debug!(error = %e, request = kind, "game frame refused");
            self.push(sock, Out::Plain(frames::error(e.frame_code(), kind)));
        }
    }

    fn touch(&mut self) {
        if let Some(g) = &mut self.game {
            g.idle_at = Instant::now() + IDLE_LIMIT;
        }
    }

    fn host_game(&mut self, role: Role, user: &str) -> AppResult<&mut Game> {
        if role != Role::Host {
            return Err(AppError::Forbidden);
        }
        match &mut self.game {
            Some(g) if g.host == user => Ok(g),
            Some(_) => Err(AppError::Forbidden),
            None => Err(AppError::NotFound("game".into())),
        }
    }

    /// `quest` is `(seconds, flags)` for a quest game, `None` for a race.
    async fn start(
        &mut self,
        sock: u64,
        role: Role,
        user: &str,
        quiz_id: i64,
        quest: Option<(u32, u32)>,
    ) -> AppResult<()> {
        if role != Role::Host {
            return Err(AppError::Forbidden);
        }
        if self.game.is_some() || self.quest.is_some() {
            return Err(AppError::Conflict("a game is already running here".into()));
        }
        let (rc, cid) = self
            .socks
            .get(&sock)
            .map(|s| (s.rc, s.cid.clone()))
            .ok_or(AppError::Forbidden)?;
        if rc <= self.now_wall() {
            return Err(AppError::Conflict("the room is closed".into()));
        }
        self.deps.rooms.try_begin(&self.sid, user, quiz_id)?;
        let opened = match quest {
            None => self.open_lobby(user, quiz_id, &cid, rc).await,
            Some((seconds, flags)) => {
                self.open_quest_lobby(user, quiz_id, &cid, rc, seconds, flags)
                    .await
            }
        };
        match opened {
            Ok(()) => Ok(()),
            Err(e) => {
                self.deps.rooms.game_ended(&self.sid);
                Err(e)
            }
        }
    }

    async fn open_lobby(&mut self, host: &str, quiz_id: i64, cid: &str, rc: i64) -> AppResult<()> {
        let quiz = self.deps.store.load_quiz(quiz_id, host.to_string()).await?;
        if quiz.questions.is_empty() {
            return Err(AppError::BadRequest("the quiz has no questions".into()));
        }
        let id = new_id();
        let now = Instant::now();
        let wall = self.wall(now);
        self.deps
            .store
            .create_game(NewGame {
                id,
                quiz_id,
                session_id: self.sid.clone(),
                class_id: cid.to_string(),
                host_id: host.to_string(),
                started_at: wall,
                mode: 0,
            })
            .await?;
        let rc_at = now + Duration::from_millis((rc - wall).max(0) as u64);
        let mut game = Game {
            id,
            title: quiz.title.clone(),
            host: host.to_string(),
            questions: quiz.questions.clone(),
            phase: Phase::Lobby,
            qi: 0,
            q_started: now,
            q_deadline: now,
            reveal_at: now,
            players: Vec::new(),
            by_user: HashMap::new(),
            active_count: 0,
            answered: 0,
            choices: Vec::new(),
            new_names: Vec::new(),
            standings: Vec::new(),
            pending: Vec::new(),
            rc_at,
            idle_at: now + IDLE_LIMIT,
            reveal: None,
            stored: false,
            podium: None,
        };
        // Everyone already in the room is in the game.
        // (in the order they arrived, so "who joined first" means something)
        let mut full = Vec::new();
        let mut arrived: Vec<u64> = self.socks.keys().copied().collect();
        arrived.sort_unstable();
        for sid in arrived {
            let Some(s) = self.socks.get_mut(&sid) else {
                continue;
            };
            if s.role == Role::Player {
                match join(&mut game, &s.user, &s.name, wall) {
                    Ok(pid) => s.pid = Some(pid),
                    Err(_) => full.push(sid),
                }
            }
        }
        self.game = Some(game);
        for sock in full {
            self.push(sock, Out::Close(1013, "full"));
        }
        let g = self.game.as_ref().expect("just set");
        let frame = frames::lobby(wall, &g.title, g.questions.len(), g.active_count);
        let has_names = !g.new_names.is_empty();
        self.broadcast_shared(frame);
        if has_names {
            self.schedule_flush();
        }
        Ok(())
    }
}

/// Put a player in the roster (or bring one back), returning their index.
fn join(g: &mut Game, user: &str, name: &str, wall: i64) -> AppResult<usize> {
    let first = match g.phase {
        Phase::Lobby => 0,
        Phase::Question | Phase::Closing | Phase::Reveal => g.qi + 1,
        Phase::Podium => g.questions.len(),
    };
    if let Some(&pid) = g.by_user.get(user) {
        let p = &mut g.players[pid];
        p.live += 1;
        if !p.active {
            p.active = true;
            p.first_question = first;
            g.active_count += 1;
            g.new_names.push(p.name.clone());
        }
        return Ok(pid);
    }
    if g.active_count >= MAX_PLAYERS_PER_GAME {
        return Err(AppError::Conflict("the game is full".into()));
    }
    let pid = g.players.len();
    g.players.push(Player {
        id: new_id(),
        user: Some(user.to_string()),
        name: name.to_string(),
        joined_at: wall,
        first_question: first,
        score: 0,
        total_ms: 0,
        correct_count: 0,
        streak: 0,
        best_streak: 0,
        answered: 0,
        live: 1,
        active: true,
        dirty: true,
        rank: 0,
        answer: None,
    });
    g.by_user.insert(user.to_string(), pid);
    g.active_count += 1;
    g.new_names.push(name.to_string());
    Ok(pid)
}

fn option_mask(q: &QuestionWithOptions, pick: impl Fn(&crate::model::OptionRow) -> bool) -> u32 {
    q.options
        .iter()
        .enumerate()
        .filter(|(_, o)| pick(o))
        .fold(0, |m, (i, _)| m | (1 << i))
}

fn kind_code(k: QuestionKind) -> u8 {
    match k {
        QuestionKind::Single => 0,
        QuestionKind::Multi => 1,
        QuestionKind::Boolean => 2,
    }
}

impl Room {
    /// The frame that brings a socket up to date with the room.
    fn sync_frame(&self, sock: u64) -> Out {
        let now = self.now_wall();
        let Some(g) = &self.game else {
            return Out::Plain(frames::none(now));
        };
        let s = &self.socks[&sock];
        let player = s.pid.map(|p| &g.players[p]);
        match g.phase {
            Phase::Lobby => Out::Plain(frames::lobby(
                now,
                &g.title,
                g.questions.len(),
                g.active_count,
            )),
            Phase::Question => {
                let head = self.question_head(g, now);
                match player {
                    Some(p) => Out::Open(
                        Utf8Bytes::from(head),
                        Tail::Mask(p.answer.as_ref().map_or(0, |a| a.mask)),
                    ),
                    None => Out::Plain(frames::close(head)),
                }
            }
            Phase::Closing => Out::Plain(frames::closed(g.qi + 1)),
            Phase::Reveal => match (&g.reveal, player) {
                (Some(head), Some(p)) => Out::Open(head.clone(), reveal_tail(p)),
                (Some(head), None) => Out::Plain(close_shared(head)),
                (None, _) => Out::Plain(frames::none(now)),
            },
            Phase::Podium => match (&g.podium, player) {
                (Some((head, _)), Some(p)) => Out::Open(
                    head.clone(),
                    Tail::Podium {
                        score: p.score,
                        rank: p.rank,
                    },
                ),
                (Some((_, host)), None) => Out::Plain(host.clone()),
                (None, _) => Out::Plain(frames::none(now)),
            },
        }
    }

    fn question_head(&self, g: &Game, now: i64) -> String {
        let q = &g.questions[g.qi];
        let options: Vec<&str> = q.options.iter().map(|o| o.text.as_str()).collect();
        frames::question_head(
            now,
            g.qi + 1,
            g.questions.len(),
            self.wall(g.q_deadline),
            kind_code(q.question.kind),
            &q.question.text,
            &options,
        )
    }

    // ----------------------------------------------------- throttled counts

    pub(super) fn mark_count_dirty(&mut self) {
        self.count_dirty = true;
        self.schedule_flush();
    }

    fn mark_progress_dirty(&mut self) {
        self.progress_dirty = true;
        self.schedule_flush();
    }

    /// Send the pending count updates now, or at the end of the 500 ms.
    pub(super) fn schedule_flush(&mut self) {
        let due = self.last_flush + PROGRESS_EVERY;
        if due <= Instant::now() {
            self.flush();
        } else if self.flush_at.is_none() {
            self.flush_at = Some(due);
        }
    }

    fn flush(&mut self) {
        if self.quest.is_some() {
            return self.quest_flush();
        }
        self.flush_at = None;
        self.last_flush = Instant::now();
        let (progress, count) = (self.progress_dirty, self.count_dirty);
        self.progress_dirty = false;
        self.count_dirty = false;
        let Some(g) = &mut self.game else { return };
        let names = std::mem::take(&mut g.new_names);
        let frame = match g.phase {
            Phase::Lobby if count => Some((true, frames::count(g.active_count))),
            Phase::Question if progress || count => Some((
                false,
                frames::progress(g.answered, g.active_count, &g.choices, &g.provisional_top()),
            )),
            _ => None,
        };
        match frame {
            Some((true, frame)) => self.broadcast_shared(frame),
            Some((false, frame)) => self.broadcast_hosts(frame),
            None => {}
        }
        if !names.is_empty() {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            self.broadcast_hosts(frames::names(&refs));
        }
    }

    /// What a host that just said hello needs besides the state frame: the
    /// names of everyone in the room (once), and the live selections of an
    /// open question.
    fn send_host_catch_up(&mut self, sock: u64) {
        let Some(g) = &self.game else { return };
        let all: Vec<&str> = g
            .players
            .iter()
            .filter(|p| p.active && !p.name.is_empty())
            .take(MAX_PLAYERS_PER_GAME)
            .map(|p| p.name.as_str())
            .collect();
        let mut frames_out = vec![frames::names(&all)];
        if g.phase == Phase::Question {
            frames_out.push(frames::progress(
                g.answered,
                g.active_count,
                &g.choices,
                &g.provisional_top(),
            ));
        }
        for f in frames_out {
            self.push(sock, Out::Plain(f));
        }
    }

    // ----------------------------------------------------------- the game

    async fn next(&mut self, role: Role, user: &str) -> AppResult<()> {
        let g = self.host_game(role, user)?;
        let (phase, qi, total) = (g.phase, g.qi, g.questions.len());
        self.touch();
        match phase {
            Phase::Lobby => self.open_question(0),
            Phase::Question => self.close_question().await,
            Phase::Closing => self.reveal_question().await,
            Phase::Reveal if qi + 1 < total => self.open_question(qi + 1),
            Phase::Reveal => self.open_podium().await,
            Phase::Podium => self.finish(EndReason::Host).await,
        }
        Ok(())
    }

    /// The host ends the game. Before anything was played that ends it at
    /// once; during play it goes to the podium exactly as the natural end
    /// does (the open question is closed and scored first, the results are
    /// stored once); from the podium it ends the game.
    async fn end(&mut self, role: Role, user: &str) -> AppResult<()> {
        let g = self.host_game(role, user)?;
        let phase = g.phase;
        self.touch();
        match phase {
            Phase::Lobby | Phase::Podium => self.finish(EndReason::Host).await,
            _ => self.show_podium(phase).await,
        }
        Ok(())
    }

    /// Stop a game that is being played and show the podium: the open
    /// question is closed and scored without a closed frame or a reveal, the
    /// results are stored once. A lobby or a podium is left as it is.
    async fn show_podium(&mut self, phase: Phase) {
        match phase {
            Phase::Question | Phase::Closing => {
                if let Some(g) = &mut self.game {
                    g.phase = Phase::Closing;
                }
                self.score_question(false).await;
                self.open_podium().await;
            }
            Phase::Reveal => self.open_podium().await,
            Phase::Lobby | Phase::Podium => {}
        }
    }

    /// The host has been away too long: what was played goes to the podium
    /// and is stored once, then the game ends with [`EndReason::HostLeft`].
    pub(super) async fn end_host_left(&mut self) {
        if self.quest.is_some() {
            return self.quest_host_left().await;
        }
        let Some(phase) = self.game.as_ref().map(|g| g.phase) else {
            return;
        };
        self.show_podium(phase).await;
        self.finish(EndReason::HostLeft).await;
    }

    fn open_question(&mut self, qi: usize) {
        let now = Instant::now();
        let wall = self.wall(now);
        let Some(g) = &mut self.game else { return };
        let limit = Duration::from_secs(g.questions[qi].question.time_limit_sec as u64);
        g.phase = Phase::Question;
        g.qi = qi;
        g.q_started = now;
        g.q_deadline = now + limit;
        g.answered = 0;
        g.choices = vec![0; g.questions[qi].options.len()];
        g.reveal = None;
        for p in &mut g.players {
            p.answer = None;
        }
        let frame = frames::close(self.question_head(self.game.as_ref().expect("game"), wall));
        self.broadcast_shared(frame);
        self.progress_dirty = false;
    }

    async fn answer(
        &mut self,
        sock: u64,
        role: Role,
        q: u32,
        mask: u32,
        at: Instant,
    ) -> AppResult<()> {
        if role != Role::Player {
            return Err(AppError::Forbidden);
        }
        let pid = self
            .socks
            .get(&sock)
            .and_then(|s| s.pid)
            .ok_or(AppError::Forbidden)?;
        // timed from when the socket received the frame, not from when the
        // room got to it
        let now = at;
        let g = self
            .game
            .as_mut()
            .ok_or_else(|| AppError::NotFound("game".into()))?;
        if g.phase != Phase::Question || now >= g.q_deadline || q as usize != g.qi + 1 {
            return Err(AppError::Conflict("that question is not open".into()));
        }
        let question = &g.questions[g.qi];
        let p = &mut g.players[pid];
        if p.first_question > g.qi {
            return Err(AppError::Conflict(
                "you joined after this question opened".into(),
            ));
        }
        if p.answer.is_some() {
            return Err(AppError::Conflict("already answered".into()));
        }
        let n = question.options.len();
        let one_only = question.question.kind != QuestionKind::Multi;
        if mask == 0 || mask >= (1 << n) || (one_only && mask.count_ones() != 1) {
            return Err(AppError::BadRequest("not a valid choice of options".into()));
        }
        let chosen: Vec<i64> = question
            .options
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, o)| o.id)
            .collect();
        let graded = grade_question(question, &chosen);
        let limit_ms = question.question.time_limit_sec * 1000;
        let elapsed = (now - g.q_started).as_millis() as i64;
        // Scored now, with what the close will use: the speed part from the
        // server's receive time, and the streak bonus from the streak the
        // player had before this question. The close only applies it.
        let bonus = if graded.correct {
            streak_bonus(p.streak + 1)
        } else {
            0
        };
        p.answer = Some(Answer {
            question_id: question.question.id,
            points: points_for(graded.credit, elapsed, limit_ms) + bonus,
            bonus,
            selected: graded.selected,
            mask,
            correct: graded.correct,
            credit: graded.credit,
            time_ms: elapsed.min(limit_ms),
        });
        g.answered += 1;
        for (bit, c) in g.choices.iter_mut().enumerate() {
            *c += mask >> bit & 1;
        }
        g.idle_at = now + IDLE_LIMIT;
        metrics::counter!("quiz_answers_total").increment(1);
        self.push(sock, Out::Plain(frames::ack()));
        self.mark_progress_dirty();
        if self.all_answered() {
            self.close_question().await;
        }
        Ok(())
    }

    /// Close the open question: lock it, tell everyone with a tiny frame, and
    /// leave the reveal for a beat later (or the host's `next`).
    async fn close_question(&mut self) {
        let Some(g) = &mut self.game else { return };
        if g.phase != Phase::Question {
            return;
        }
        g.phase = Phase::Closing;
        g.reveal_at = Instant::now() + Duration::from_millis(REVEAL_DELAY_MS);
        let frame = frames::closed(g.qi + 1);
        self.broadcast_shared(frame);
    }

    /// Reveal a closed question: score the answers, rank the players, send
    /// the reveal and write the answers in one transaction.
    async fn reveal_question(&mut self) {
        self.score_question(true).await;
    }

    /// Score a closed question and write its answers; with `announce` the
    /// reveal goes out to every socket.
    async fn score_question(&mut self, announce: bool) {
        let started = std::time::Instant::now();
        let Some(g) = &mut self.game else { return };
        if g.phase != Phase::Closing {
            return;
        }
        g.phase = Phase::Reveal;
        let question = g.questions[g.qi].clone();
        let mut counts = vec![0u32; question.options.len()];
        let limit_ms = question.question.time_limit_sec * 1000;
        for p in &mut g.players {
            let Some(a) = p.answer.as_ref() else {
                // Not answering costs the whole limit in the tie-break, so it
                // never beats a player who tried, and it ends the streak.
                if p.active && p.first_question <= g.qi {
                    p.total_ms += limit_ms;
                    p.streak = 0;
                    p.dirty = true;
                }
                continue;
            };
            for (bit, c) in counts.iter_mut().enumerate() {
                *c += a.mask >> bit & 1;
            }
            // Only a fully right answer keeps a streak going; a wrong or a
            // partly right one ends it. The points and the bonus were
            // settled when the answer arrived.
            p.streak = if a.correct { p.streak + 1 } else { 0 };
            p.best_streak = p.best_streak.max(p.streak);
            p.answered += 1;
            p.score += a.points;
            p.total_ms += a.time_ms;
            p.correct_count += a.correct as i64;
            p.dirty = true;
            g.pending.push(AnswerWrite {
                id: new_id(),
                player_id: p.id,
                question_id: a.question_id,
                selected: a.selected.clone(),
                correct: a.correct,
                credit: a.credit,
                points: a.points,
                bonus: a.bonus,
                time_ms: a.time_ms,
            });
        }
        rank(g);
        let head = frames::reveal_head(
            g.qi + 1,
            option_mask(&question, |o| o.is_correct),
            &counts,
            question.question.explanation.as_deref(),
            &g.rows(5),
        );
        let host_frame = frames::close(head.clone());
        let head = Utf8Bytes::from(head);
        g.reveal = Some(head.clone());
        if announce {
            self.broadcast(|s, room| {
                let g = room.game.as_ref()?;
                Some(match s.pid {
                    Some(pid) => Out::Open(head.clone(), reveal_tail(&g.players[pid])),
                    None => Out::Plain(host_frame.clone()),
                })
            });
        }
        self.persist(None).await;
        metrics::histogram!("quiz_question_close_seconds").record(started.elapsed().as_secs_f64());
    }

    /// The podium: the final standings go out and the results are stored,
    /// once. The game itself ends when the host leaves the podium (or the room
    /// closes, or it has been idle).
    async fn open_podium(&mut self) {
        let Some(g) = &mut self.game else { return };
        g.phase = Phase::Podium;
        rank(g);
        let top = g.rows(3);
        let player_head = Utf8Bytes::from(frames::podium_head(&top, None));
        let host_frame = frames::close(frames::podium_head(&top, Some(&g.rows(usize::MAX))));
        g.podium = Some((player_head.clone(), host_frame.clone()));
        self.broadcast(|s, room| {
            let g = room.game.as_ref()?;
            Some(match s.pid {
                Some(pid) => {
                    let p = &g.players[pid];
                    Out::Open(
                        player_head.clone(),
                        Tail::Podium {
                            score: p.score,
                            rank: p.rank,
                        },
                    )
                }
                None => Out::Plain(host_frame.clone()),
            })
        });
        let ended_at = self.now_wall();
        if self.persist(Some(ended_at)).await
            && let Some(g) = &mut self.game
        {
            g.stored = true;
        }
    }

    /// End the game: write everything that is left, tell everyone. The sockets stay: the room can host another game.
    pub(super) async fn finish(&mut self, reason: EndReason) {
        if self.quest.is_some() {
            return self.quest_finish(reason).await;
        }
        if self.game.is_none() {
            return;
        }
        if self
            .game
            .as_ref()
            .is_some_and(|g| g.phase == Phase::Closing)
        {
            // the answers of a closed question are scored before the end
            self.reveal_question().await;
        }
        let ended_at = self.now_wall();
        // the results were stored when the podium opened
        if !self.game.as_ref().is_some_and(|g| g.stored) {
            self.persist(Some(ended_at)).await;
        }
        self.game = None;
        for s in self.socks.values_mut() {
            s.pid = None;
        }
        self.deps.rooms.game_ended(&self.sid);
        self.progress_dirty = false;
        self.count_dirty = false;
        self.broadcast_shared(frames::ended(reason));
    }

    /// Write the dirty players and the answers waiting (and the end of the
    /// game). A failed write keeps them for the next one.
    async fn persist(&mut self, ended_at: Option<i64>) -> bool {
        let Some(g) = &mut self.game else { return true };
        let players: Vec<PlayerWrite> = g
            .players
            .iter()
            .filter(|p| p.dirty && p.active)
            .map(|p| PlayerWrite {
                id: p.id,
                user_id: p.user.clone(),
                name: p.name.clone(),
                joined_at: p.joined_at,
                score: p.score,
                total_ms: p.total_ms,
                correct_count: p.correct_count,
                answered: i64::from(p.answered),
                best_streak: i64::from(p.best_streak),
            })
            .collect();
        let written: Vec<i64> = players.iter().map(|p| p.id).collect();
        let batch = Batch {
            game_id: g.id,
            players,
            answers: std::mem::take(&mut g.pending),
            ended_at,
        };
        if batch.players.is_empty() && batch.answers.is_empty() && ended_at.is_none() {
            return true;
        }
        match self.deps.store.write(batch.clone()).await {
            Ok(()) => {
                if let Some(g) = &mut self.game {
                    for p in g.players.iter_mut().filter(|p| written.contains(&p.id)) {
                        p.dirty = false;
                    }
                }
                true
            }
            Err(e) => {
                tracing::error!(error = %e, game = batch.game_id, "writing the game failed");
                if let Some(g) = &mut self.game {
                    g.pending = batch.answers;
                }
                false
            }
        }
    }
}

/// Order the active players and set their ranks.
fn rank(g: &mut Game) {
    let mut order: Vec<usize> = (0..g.players.len())
        .filter(|&i| g.players[i].active)
        .collect();
    order.sort_by(|&a, &b| {
        let (pa, pb) = (&g.players[a], &g.players[b]);
        pb.score
            .cmp(&pa.score)
            .then(pa.total_ms.cmp(&pb.total_ms))
            .then(pa.joined_at.cmp(&pb.joined_at))
            .then(a.cmp(&b))
    });
    for (i, &pid) in order.iter().enumerate() {
        g.players[pid].rank = i as u32 + 1;
    }
    g.standings = order;
}

fn reveal_tail(p: &Player) -> Tail {
    let a = p.answer.as_ref();
    Tail::Reveal {
        score: p.score,
        rank: p.rank,
        points: a.map_or(0, |a| a.points),
        correct: a.is_some_and(|a| a.correct),
        mask: a.map_or(0, |a| a.mask),
        streak: p.streak,
        bonus: a.map_or(0, |a| a.bonus),
    }
}

/// A shared open prefix as a complete frame.
fn close_shared(head: &Utf8Bytes) -> Utf8Bytes {
    frames::close(head.to_string())
}
