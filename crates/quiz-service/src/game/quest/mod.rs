//! Quest mode: a self-paced quiz game with coins, chests and steals
//! (`QUIZ-QUEST.md` in the design, `PROTOCOL.md` for the frames).
//!
//! Everyone answers at their own speed from their own shuffled queue. A right
//! answer earns coins and a treasure pick; a wrong or missed one costs a short
//! pause. The state of every player lives here, in memory, on the room task;
//! the room hands in the client frames and the clock and sends what comes out.
//!
//! The mode is a module of its own: [`Quest`] is plain state with methods that
//! take `now` and collect what to send in a [`Fx`]; the `impl Room` at the end
//! is the thin layer that connects it to sockets, timers and the store.

pub mod chest;
pub mod frames;
#[cfg(test)]
mod tests;

use self::chest::{Draw, Outcome, draw_chests, steal_amount};
use self::frames::{Event, Pending};
use super::frames::{self as race, EndReason, Out, Tail};
use super::room::{IDLE_LIMIT, Room, Sock};
use crate::auth::Role;
use crate::error::{AppError, AppResult};
use crate::grading::grade_question;
use crate::ids::new_id;
use crate::model::{QuestionKind, QuestionWithOptions};
use crate::repo::games::{Batch, NewGame, PlayerWrite};
use crate::repo::quizzes::limits::MAX_PLAYERS_PER_GAME;
use axum::extract::ws::Utf8Bytes;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// The pause after a wrong or missed answer, enforced by the server.
pub const PAUSE_MS: u64 = 2500;
/// Characters a player can choose from.
pub const CHARACTERS: u32 = 12;
/// The standings strip of a player is refreshed at most this often.
pub const STANDINGS_EVERY: Duration = Duration::from_secs(3);
/// Rows of the player's standings strip and of the host's board.
const STRIP_ROWS: usize = 5;
const BOARD_ROWS: usize = 10;
/// The lengths a game may have, in seconds (5, 10 or 15 minutes).
pub const ALLOWED_SECONDS: [u32; 3] = [300, 600, 900];
/// Bit 0 of the start flags: friendly mode (no steals).
pub const FLAG_FRIENDLY: u32 = 1;

/// How long a player has been thinking and what they are doing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PState {
    /// Before the game runs, or between a chest and the next question.
    Idle,
    /// A question is open until `deadline`.
    Asking { q: usize, deadline: Instant },
    /// The three hidden chests of a right answer.
    Chest([Outcome; 3]),
    /// Thinking about a wrong answer until the instant.
    Pause(Instant),
}

struct QPlayer {
    id: i64,
    /// `None` once the person is forgotten.
    user: Option<String>,
    name: String,
    joined_at: i64,
    character: u8,
    coins: i64,
    streak: u32,
    best_streak: u32,
    /// Right answers left that earn double.
    booster: u8,
    shield: bool,
    /// The player's questions: indexes into the quiz, shuffled.
    queue: Vec<u16>,
    qpos: usize,
    /// The last question served (1-based); every answer and pick names it.
    serial: u32,
    state: PState,
    answered: u32,
    correct: u32,
    /// Sockets of this player attached now.
    socks: Vec<u64>,
    /// In the roster (a lobby player who left is not).
    active: bool,
    rank: u32,
    sent_rank: u32,
    sent_coins: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QPhase {
    Lobby,
    Running { ends_at: Instant },
    Podium,
}

/// What to send as a result of one step.
#[derive(Default)]
pub(super) struct Fx {
    /// To the sockets of one player (an index into the players).
    to_player: Vec<(usize, Out)>,
    /// To every host socket.
    to_hosts: Vec<Utf8Bytes>,
    /// To every socket.
    to_all: Vec<Utf8Bytes>,
}

/// A quest game in progress.
pub(super) struct Quest {
    id: i64,
    title: String,
    host: String,
    questions: Arc<Vec<QuestionWithOptions>>,
    /// Each question's shared frame prefix, encoded once.
    heads: Vec<Utf8Bytes>,
    seconds: u32,
    friendly: bool,
    phase: QPhase,
    players: Vec<QPlayer>,
    by_user: HashMap<String, usize>,
    active_count: usize,
    rng: Box<dyn Draw>,
    /// When something is due: `(when, player, serial)`; stale ones are skipped.
    timers: BinaryHeap<Reverse<(Instant, usize, u32)>>,
    answers: u64,
    correct: u64,
    board_dirty: bool,
    /// Players who joined, or changed character, since the last roster frame.
    roster_new: Vec<(String, u8)>,
    next_standings: Instant,
    last_strip: String,
    podium: Option<(Utf8Bytes, Utf8Bytes)>,
    rc_at: Instant,
    idle_at: Instant,
}

impl Quest {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        id: i64,
        title: String,
        host: String,
        questions: Arc<Vec<QuestionWithOptions>>,
        seconds: u32,
        friendly: bool,
        rng: Box<dyn Draw>,
        rc_at: Instant,
        now: Instant,
    ) -> Quest {
        let heads = questions
            .iter()
            .map(|q| {
                let options: Vec<&str> = q.options.iter().map(|o| o.text.as_str()).collect();
                Utf8Bytes::from(frames::question_head(
                    kind_code(q.question.kind),
                    &q.question.text,
                    &options,
                    q.question.time_limit_sec,
                ))
            })
            .collect();
        Quest {
            id,
            title,
            host,
            questions,
            heads,
            seconds,
            friendly,
            phase: QPhase::Lobby,
            players: Vec::new(),
            by_user: HashMap::new(),
            active_count: 0,
            rng,
            timers: BinaryHeap::new(),
            answers: 0,
            correct: 0,
            board_dirty: false,
            roster_new: Vec::new(),
            next_standings: now,
            last_strip: String::new(),
            podium: None,
            rc_at,
            idle_at: now + IDLE_LIMIT,
        }
    }

    /// Put a player in the roster (or bring one back), returning their index.
    pub(super) fn join(&mut self, user: &str, name: &str, wall: i64) -> AppResult<usize> {
        if let Some(&pid) = self.by_user.get(user) {
            let p = &mut self.players[pid];
            if !p.active {
                p.active = true;
                self.active_count += 1;
                self.roster_new.push((p.name.clone(), p.character));
            }
            return Ok(pid);
        }
        if self.active_count >= MAX_PLAYERS_PER_GAME {
            return Err(AppError::Conflict("the game is full".into()));
        }
        let pid = self.players.len();
        // Characters are a small number; a new player starts with one that
        // depends on how many came before, until they choose.
        let character = (pid as u32 % CHARACTERS) as u8;
        let mut queue: Vec<u16> = (0..self.questions.len() as u16).collect();
        shuffle(&mut *self.rng, &mut queue);
        self.players.push(QPlayer {
            id: new_id(),
            user: Some(user.to_string()),
            name: name.to_string(),
            joined_at: wall,
            character,
            coins: 0,
            streak: 0,
            best_streak: 0,
            booster: 0,
            shield: false,
            queue,
            qpos: 0,
            serial: 0,
            state: PState::Idle,
            answered: 0,
            correct: 0,
            socks: Vec::new(),
            active: true,
            rank: 0,
            sent_rank: 0,
            sent_coins: i64::MIN,
        });
        self.by_user.insert(user.to_string(), pid);
        self.active_count += 1;
        self.roster_new.push((name.to_string(), character));
        self.board_dirty = true;
        Ok(pid)
    }

    fn running(&self) -> Option<Instant> {
        match self.phase {
            QPhase::Running { ends_at } => Some(ends_at),
            _ => None,
        }
    }

    /// Active players best first: by coins, then who joined first.
    fn order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.players.len())
            .filter(|&i| self.players[i].active)
            .collect();
        order.sort_by(|&a, &b| {
            let (pa, pb) = (&self.players[a], &self.players[b]);
            pb.coins
                .cmp(&pa.coins)
                .then(pa.joined_at.cmp(&pb.joined_at))
                .then(a.cmp(&b))
        });
        order
    }

    fn rows(&self, order: &[usize], take: usize) -> Vec<(&str, u8, i64)> {
        order
            .iter()
            .take(take)
            .map(|&i| {
                let p = &self.players[i];
                (p.name.as_str(), p.character, p.coins)
            })
            .collect()
    }

    /// The game starts: every player gets their first question.
    pub(super) fn begin(&mut self, now: Instant, wall: i64, fx: &mut Fx) {
        let ends_at = now + Duration::from_secs(u64::from(self.seconds));
        self.phase = QPhase::Running { ends_at };
        self.next_standings = now + STANDINGS_EVERY;
        fx.to_all
            .push(frames::running(wall, wall + i64::from(self.seconds) * 1000));
        for pid in 0..self.players.len() {
            if self.players[pid].active {
                self.serve(pid, now, fx);
            }
        }
        self.board_dirty = true;
    }

    /// Serve a player their next question.
    fn serve(&mut self, pid: usize, now: Instant, fx: &mut Fx) {
        let n = self.questions.len();
        let last = self.players[pid]
            .qpos
            .checked_sub(1)
            .and_then(|i| self.players[pid].queue.get(i).copied());
        if self.players[pid].qpos >= self.players[pid].queue.len() {
            let mut queue: Vec<u16> = (0..n as u16).collect();
            shuffle(&mut *self.rng, &mut queue);
            // a new round never starts with the question just asked
            if n > 1 && Some(queue[0]) == last {
                let j = 1 + self.rng.below(n as u64 - 1) as usize;
                queue.swap(0, j);
            }
            let p = &mut self.players[pid];
            p.queue = queue;
            p.qpos = 0;
        }
        let p = &mut self.players[pid];
        let q = p.queue[p.qpos] as usize;
        p.qpos += 1;
        p.serial += 1;
        let limit = Duration::from_secs(self.questions[q].question.time_limit_sec as u64);
        let deadline = now + limit;
        p.state = PState::Asking { q, deadline };
        self.timers.push(Reverse((deadline, pid, p.serial)));
        fx.to_player.push((
            pid,
            Out::Open(
                self.heads[q].clone(),
                Tail::Serial {
                    serial: p.serial,
                    remaining_ms: limit.as_millis() as i64,
                },
            ),
        ));
    }

    /// The question is answered or missed in the wrong way: the pause.
    fn start_pause(&mut self, pid: usize, now: Instant) {
        let until = now + Duration::from_millis(PAUSE_MS);
        let p = &mut self.players[pid];
        p.state = PState::Pause(until);
        self.timers.push(Reverse((until, pid, p.serial)));
    }

    fn verdict_wrong(&self, pid: usize, q: usize, result: u8) -> Out {
        let question = &self.questions[q];
        let mask = question
            .options
            .iter()
            .enumerate()
            .filter(|(_, o)| o.is_correct)
            .fold(0u32, |m, (i, _)| m | (1 << i));
        Out::Plain(frames::verdict_wrong(
            self.players[pid].serial,
            result,
            self.players[pid].coins,
            mask,
            PAUSE_MS as i64,
            question.question.explanation.as_deref(),
        ))
    }

    /// A player answers the question they were served.
    pub(super) fn answer(
        &mut self,
        pid: usize,
        serial: u32,
        mask: u32,
        now: Instant,
        fx: &mut Fx,
    ) -> AppResult<()> {
        if self.running().is_none() {
            return Err(AppError::Conflict("the game is not running".into()));
        }
        self.expire(pid, now, fx);
        let PState::Asking { q, deadline } = self.players[pid].state else {
            return Err(AppError::Conflict("there is no open question".into()));
        };
        if serial != self.players[pid].serial {
            return Err(AppError::Conflict(
                "that is not the current question".into(),
            ));
        }
        let question = self.questions[q].clone();
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
        // exactly the right set; partial credit does not apply here
        let right = grade_question(&question, &chosen).correct;
        self.answers += 1;
        self.board_dirty = true;
        self.players[pid].answered += 1;
        if !right {
            let p = &mut self.players[pid];
            p.streak = 0;
            fx.to_player.push((pid, self.verdict_wrong(pid, q, 0)));
            self.start_pause(pid, now);
            return Ok(());
        }
        self.correct += 1;
        let limit_ms = question.question.time_limit_sec * 1000;
        let remaining_ms = deadline.saturating_duration_since(now).as_millis() as i64;
        let speed = 10 * remaining_ms.clamp(0, limit_ms) / limit_ms;
        let p = &mut self.players[pid];
        p.correct += 1;
        p.streak += 1;
        p.best_streak = p.best_streak.max(p.streak);
        let bonus = (5 * (i64::from(p.streak) - 1)).min(25);
        let mut earned = 10 + speed + bonus;
        let mut booster_ended = false;
        if p.booster > 0 {
            earned *= 2;
            p.booster -= 1;
            booster_ended = p.booster == 0;
        }
        p.coins += earned;
        let streak = p.streak;
        // a steal needs somebody to steal from
        let allow_steal = !self.friendly && self.active_count >= 2;
        let outcomes = draw_chests(&mut *self.rng, streak, allow_steal);
        let p = &mut self.players[pid];
        p.state = PState::Chest(outcomes);
        fx.to_player.push((
            pid,
            Out::Plain(frames::verdict_right(
                p.serial, earned, streak, bonus, p.coins, p.booster,
            )),
        ));
        if booster_ended {
            fx.to_player
                .push((pid, Out::Plain(frames::event(&Event::BoosterEnded))));
        }
        Ok(())
    }

    /// A player opens one of their three chests.
    pub(super) fn pick(
        &mut self,
        pid: usize,
        serial: u32,
        chest_index: u32,
        now: Instant,
        fx: &mut Fx,
    ) -> AppResult<()> {
        if self.running().is_none() {
            return Err(AppError::Conflict("the game is not running".into()));
        }
        let PState::Chest(outcomes) = self.players[pid].state else {
            return Err(AppError::Conflict("there is no chest to pick".into()));
        };
        if serial != self.players[pid].serial {
            return Err(AppError::Conflict("that is not the current chest".into()));
        }
        if chest_index > 2 {
            return Err(AppError::BadRequest("there are three chests".into()));
        }
        let picked = outcomes[chest_index as usize];
        let mut effect = picked.wire();
        let mut victim_name: Option<String> = None;
        let mut blocked = false;
        match picked {
            Outcome::Coins(n) => self.players[pid].coins += n,
            Outcome::Jackpot => self.players[pid].coins += chest::JACKPOT,
            Outcome::Booster => self.players[pid].booster = chest::BOOSTER_ANSWERS,
            Outcome::Shield => self.players[pid].shield = true,
            Outcome::Slip => {
                let p = &mut self.players[pid];
                let lost = chest::SLIP.min(p.coins);
                p.coins -= lost;
                effect = (5, lost);
            }
            Outcome::Steal => match self.steal_target(pid) {
                None => effect = (4, 0),
                Some(v) => {
                    let thief = self.players[pid].name.clone();
                    victim_name = Some(self.players[v].name.clone());
                    if self.players[v].shield {
                        self.players[v].shield = false;
                        blocked = true;
                        effect = (4, 0);
                        fx.to_player.push((
                            v,
                            Out::Plain(frames::event(&Event::Blocked { thief: &thief })),
                        ));
                    } else {
                        let amount = steal_amount(self.players[v].coins);
                        self.players[v].coins -= amount;
                        self.players[pid].coins += amount;
                        effect = (4, amount);
                        fx.to_player.push((
                            v,
                            Out::Plain(frames::event(&Event::Stolen {
                                thief: &thief,
                                amount,
                                coins: self.players[v].coins,
                            })),
                        ));
                    }
                }
            },
        }
        fx.to_player.push((
            pid,
            Out::Plain(frames::chest(
                serial,
                chest_index as usize,
                effect,
                self.players[pid].coins,
                &outcomes,
                victim_name.as_deref(),
                blocked,
            )),
        ));
        self.board_dirty = true;
        self.players[pid].state = PState::Idle;
        self.serve(pid, now, fx);
        Ok(())
    }

    /// Who a steal takes from: a random player ranked above the thief, or the
    /// runner-up when the thief leads.
    fn steal_target(&mut self, thief: usize) -> Option<usize> {
        let order = self.order();
        let at = order.iter().position(|&i| i == thief)?;
        if at > 0 {
            let pick = self.rng.below(at as u64) as usize;
            Some(order[pick])
        } else {
            order.get(1).copied()
        }
    }

    /// A player asks for the question again, or for the next one. Only a
    /// question already served can be asked for; a pause or a chest in the
    /// way is refused.
    pub(super) fn ask_again(&mut self, pid: usize, now: Instant, fx: &mut Fx) -> AppResult<()> {
        if self.running().is_none() {
            return Err(AppError::Conflict("the game is not running".into()));
        }
        self.expire(pid, now, fx);
        match self.players[pid].state {
            PState::Asking { q, deadline } => {
                let remaining = deadline.saturating_duration_since(now).as_millis() as i64;
                fx.to_player.push((
                    pid,
                    Out::Open(
                        self.heads[q].clone(),
                        Tail::Serial {
                            serial: self.players[pid].serial,
                            remaining_ms: remaining,
                        },
                    ),
                ));
                Ok(())
            }
            PState::Pause(_) => Err(AppError::Conflict("wait for the pause to end".into())),
            PState::Chest(_) => Err(AppError::Conflict("pick a chest first".into())),
            PState::Idle => {
                self.serve(pid, now, fx);
                Ok(())
            }
        }
    }

    /// A question whose time is over counts as missed, whenever somebody
    /// notices.
    fn expire(&mut self, pid: usize, now: Instant, fx: &mut Fx) {
        if let PState::Asking { q, deadline } = self.players[pid].state
            && now >= deadline
        {
            self.players[pid].streak = 0;
            fx.to_player.push((pid, self.verdict_wrong(pid, q, 2)));
            self.start_pause(pid, now);
            self.board_dirty = true;
        }
    }

    pub(super) fn character(&mut self, pid: usize, character: u32) -> AppResult<()> {
        if character >= CHARACTERS {
            return Err(AppError::BadRequest("there are twelve characters".into()));
        }
        if self.phase == QPhase::Podium {
            return Err(AppError::Conflict("the game is over".into()));
        }
        let p = &mut self.players[pid];
        if p.character != character as u8 {
            p.character = character as u8;
            self.roster_new.push((p.name.clone(), p.character));
            self.board_dirty = true;
        }
        Ok(())
    }

    /// Everything that is due: missed questions, pauses over.
    pub(super) fn tick(&mut self, now: Instant, fx: &mut Fx) {
        while let Some(&Reverse((when, pid, serial))) = self.timers.peek() {
            if when > now {
                break;
            }
            self.timers.pop();
            if self.running().is_none() || self.players[pid].serial != serial {
                continue;
            }
            match self.players[pid].state {
                PState::Asking { .. } => self.expire(pid, now, fx),
                PState::Pause(until) if now >= until => {
                    self.players[pid].state = PState::Idle;
                    self.serve(pid, now, fx);
                }
                _ => {}
            }
        }
    }

    /// The earliest moment this game needs attention.
    pub(super) fn next_wake(&self) -> Option<Instant> {
        let running = self.running();
        [
            Some(self.rc_at),
            Some(self.idle_at),
            self.timers.peek().map(|Reverse((t, _, _))| *t),
            running,
            running.map(|_| self.next_standings),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// The standings strip: one shared prefix, and each player's own place as
    /// a few bytes, sent to those for whom anything changed.
    pub(super) fn standings(&mut self, now: Instant, fx: &mut Fx) {
        self.next_standings = now + STANDINGS_EVERY;
        let order = self.order();
        for (i, &pid) in order.iter().enumerate() {
            self.players[pid].rank = i as u32 + 1;
        }
        let head = frames::standings_head(&self.rows(&order, STRIP_ROWS));
        let top_changed = head != self.last_strip;
        let shared = Utf8Bytes::from(head.clone());
        for &pid in &order {
            let p = &mut self.players[pid];
            if p.socks.is_empty() {
                continue;
            }
            if top_changed || p.rank != p.sent_rank || p.coins != p.sent_coins {
                p.sent_rank = p.rank;
                p.sent_coins = p.coins;
                fx.to_player.push((
                    pid,
                    Out::Open(
                        shared.clone(),
                        Tail::Standing {
                            rank: p.rank,
                            coins: p.coins,
                        },
                    ),
                ));
            }
        }
        self.last_strip = head;
    }

    /// The host's live board, when something changed.
    pub(super) fn board(&mut self, now: Instant) -> Option<Utf8Bytes> {
        if !self.board_dirty {
            return None;
        }
        self.board_dirty = false;
        let order = self.order();
        let ms_left = self
            .running()
            .map_or(0, |e| e.saturating_duration_since(now).as_millis() as i64);
        Some(frames::board(
            &self.rows(&order, BOARD_ROWS),
            self.active_count,
            self.answers,
            self.correct,
            ms_left,
        ))
    }

    /// The time is up: the podium.
    pub(super) fn time_up(&mut self, fx: &mut Fx) {
        self.phase = QPhase::Podium;
        let order = self.order();
        for (i, &pid) in order.iter().enumerate() {
            self.players[pid].rank = i as u32 + 1;
        }
        let top: Vec<(&str, i64)> = order
            .iter()
            .take(3)
            .map(|&i| (self.players[i].name.as_str(), self.players[i].coins))
            .collect();
        let all: Vec<(&str, i64)> = order
            .iter()
            .map(|&i| (self.players[i].name.as_str(), self.players[i].coins))
            .collect();
        let player_head = Utf8Bytes::from(race::podium_head(&top, None));
        let host_frame = race::close(race::podium_head(&top, Some(&all)));
        for &pid in &order {
            let p = &self.players[pid];
            fx.to_player.push((
                pid,
                Out::Open(
                    player_head.clone(),
                    Tail::Podium {
                        score: p.coins,
                        rank: p.rank,
                    },
                ),
            ));
        }
        fx.to_hosts.push(host_frame.clone());
        self.podium = Some((player_head, host_frame));
    }

    fn me(&self, pid: usize, now: Instant) -> Utf8Bytes {
        let p = &self.players[pid];
        let (pending, pause_ms) = match p.state {
            PState::Idle => (Pending::Nothing, 0),
            PState::Asking { .. } => (Pending::Question, 0),
            PState::Chest(_) => (Pending::Chest, 0),
            PState::Pause(until) => (
                Pending::Pause,
                until.saturating_duration_since(now).as_millis() as i64,
            ),
        };
        frames::me(
            p.coins,
            p.rank,
            p.streak,
            p.booster,
            p.shield,
            p.character,
            p.serial,
            pending,
            pause_ms,
        )
    }

    /// What a player needs to be up to date after (re)connecting.
    pub(super) fn sync_player(&mut self, pid: usize, now: Instant, wall: i64, fx: &mut Fx) {
        match self.phase {
            QPhase::Lobby => fx.to_player.push((
                pid,
                Out::Plain(frames::lobby(
                    wall,
                    &self.title,
                    self.questions.len(),
                    self.active_count,
                    self.seconds,
                    self.friendly as u32,
                )),
            )),
            QPhase::Running { ends_at } => {
                let left = ends_at.saturating_duration_since(now).as_millis() as i64;
                fx.to_player
                    .push((pid, Out::Plain(frames::running(wall, wall + left))));
                fx.to_player.push((pid, Out::Plain(self.me(pid, now))));
                if let PState::Asking { q, deadline } = self.players[pid].state {
                    fx.to_player.push((
                        pid,
                        Out::Open(
                            self.heads[q].clone(),
                            Tail::Serial {
                                serial: self.players[pid].serial,
                                remaining_ms: deadline.saturating_duration_since(now).as_millis()
                                    as i64,
                            },
                        ),
                    ));
                }
            }
            QPhase::Podium => {
                if let Some((head, _)) = &self.podium {
                    let p = &self.players[pid];
                    fx.to_player.push((
                        pid,
                        Out::Open(
                            head.clone(),
                            Tail::Podium {
                                score: p.coins,
                                rank: p.rank,
                            },
                        ),
                    ));
                }
            }
        }
    }

    /// What a host needs after (re)connecting.
    pub(super) fn sync_host(&mut self, now: Instant, wall: i64) -> Vec<Out> {
        let mut out = Vec::new();
        match self.phase {
            QPhase::Lobby => out.push(Out::Plain(frames::lobby(
                wall,
                &self.title,
                self.questions.len(),
                self.active_count,
                self.seconds,
                self.friendly as u32,
            ))),
            QPhase::Running { ends_at } => {
                let left = ends_at.saturating_duration_since(now).as_millis() as i64;
                out.push(Out::Plain(frames::running(wall, wall + left)));
                self.board_dirty = true;
            }
            QPhase::Podium => {
                if let Some((_, host)) = &self.podium {
                    out.push(Out::Plain(host.clone()));
                }
            }
        }
        let all: Vec<(&str, u8)> = self
            .players
            .iter()
            .filter(|p| p.active && !p.name.is_empty())
            .take(MAX_PLAYERS_PER_GAME)
            .map(|p| (p.name.as_str(), p.character))
            .collect();
        out.push(Out::Plain(frames::roster(&all)));
        out
    }
}

fn kind_code(k: QuestionKind) -> u8 {
    match k {
        QuestionKind::Single => 0,
        QuestionKind::Multi => 1,
        QuestionKind::Boolean => 2,
    }
}

fn shuffle<T>(rng: &mut dyn Draw, items: &mut [T]) {
    for i in (1..items.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        items.swap(i, j);
    }
}

// ------------------------------------------------------------ the room side

impl Room {
    /// Send what a step produced.
    fn apply_fx(&mut self, fx: Fx) {
        for f in fx.to_all {
            self.broadcast_shared(f);
        }
        for f in fx.to_hosts {
            self.broadcast_hosts(f);
        }
        for (pid, out) in fx.to_player {
            let socks = self
                .quest
                .as_ref()
                .map(|q| q.players[pid].socks.clone())
                .unwrap_or_default();
            for sock in socks {
                self.push(sock, out.clone());
            }
        }
    }

    fn quest_touch(&mut self) {
        if let Some(q) = &mut self.quest {
            q.idle_at = Instant::now() + IDLE_LIMIT;
        }
    }

    /// A host opens a lobby for a quest game.
    pub(super) async fn open_quest_lobby(
        &mut self,
        host: &str,
        quiz_id: i64,
        cid: &str,
        rc: i64,
        seconds: u32,
        flags: u32,
    ) -> AppResult<()> {
        if !ALLOWED_SECONDS.contains(&seconds) {
            return Err(AppError::BadRequest(
                "a quest lasts 5, 10 or 15 minutes (300, 600 or 900 seconds)".into(),
            ));
        }
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
                mode: 1,
            })
            .await?;
        let rc_at = now + Duration::from_millis((rc - wall).max(0) as u64);
        let mut quest = Quest::new(
            id,
            quiz.title.clone(),
            host.to_string(),
            quiz.questions.clone(),
            seconds,
            flags & FLAG_FRIENDLY != 0,
            (self.deps.quest_draw)(),
            rc_at,
            now,
        );
        self.spans.begin(id, &self.sid, self.deps.track_spans, now);
        let mut arrived: Vec<u64> = self.socks.keys().copied().collect();
        arrived.sort_unstable();
        let mut full = Vec::new();
        for sid in arrived {
            let Some(s) = self.socks.get_mut(&sid) else {
                continue;
            };
            self.spans.attach(&s.user, s.role, wall);
            if s.role == Role::Player {
                match quest.join(&s.user, &s.name, wall) {
                    Ok(pid) => {
                        s.pid = Some(pid);
                        quest.players[pid].socks.push(sid);
                    }
                    Err(_) => full.push(sid),
                }
            }
        }
        let frame = frames::lobby(
            wall,
            &quest.title,
            quest.questions.len(),
            quest.active_count,
            seconds,
            flags,
        );
        self.quest = Some(quest);
        for sock in full {
            self.push(sock, Out::Close(1013, "full"));
        }
        self.broadcast_shared(frame);
        self.schedule_flush();
        Ok(())
    }

    /// A socket attaches while a quest game exists.
    pub(super) fn quest_attach(&mut self, m: super::room::Member) -> AppResult<()> {
        let now = Instant::now();
        let wall = self.wall(now);
        let mut pid = None;
        let q = self.quest.as_mut().expect("a quest game");
        match m.role {
            Role::Host if q.host != m.user => return Err(AppError::Forbidden),
            Role::Player => {
                let p = q.join(&m.user, &m.name, wall)?;
                q.players[p].socks.push(m.sock);
                pid = Some(p);
            }
            _ => {}
        }
        self.spans.attach(&m.user, m.role, wall);
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
                out: m.out,
                pid,
            },
        );
        let q = self.quest.as_mut().expect("a quest game");
        let mut fx = Fx::default();
        match pid {
            Some(p) => {
                // a player coming back in the middle: their state, then
                // whatever they were doing
                q.sync_player(p, now, wall, &mut fx);
                // a first-time player joining a running game gets a question
                if q.running().is_some() && matches!(q.players[p].state, PState::Idle) {
                    q.serve(p, now, &mut fx);
                }
                q.board_dirty = true;
            }
            None => {
                for out in q.sync_host(now, wall) {
                    self.push(m.sock, out);
                }
            }
        }
        // only the socket that just came needs the player's sync
        for (_, out) in std::mem::take(&mut fx.to_player) {
            self.push(m.sock, out);
        }
        self.schedule_flush();
        Ok(())
    }

    pub(super) async fn quest_detach(&mut self, sock: u64) {
        let Some(s) = self.socks.remove(&sock) else {
            return;
        };
        let now = Instant::now();
        let wall = self.wall(now);
        self.spans.detach(&s.user, wall, now);
        if let (Some(pid), Some(q)) = (s.pid, self.quest.as_mut()) {
            let p = &mut q.players[pid];
            p.socks.retain(|&x| x != sock);
            if p.socks.is_empty() && q.phase == QPhase::Lobby && p.active {
                p.active = false;
                q.active_count -= 1;
            }
            q.board_dirty = true;
            self.schedule_flush();
        }
    }

    pub(super) async fn quest_forget(&mut self, user: &str) {
        if self.quest.as_ref().is_some_and(|q| q.host == user) {
            self.quest_finish(EndReason::Host).await;
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
        if let Some(q) = &mut self.quest
            && let Some(pid) = q.by_user.remove(user)
        {
            let p = &mut q.players[pid];
            p.user = None;
            p.name.clear();
        }
    }

    /// A client frame while a quest game exists.
    pub(super) async fn quest_frame(
        &mut self,
        sock: u64,
        role: Role,
        user: &str,
        frame: race::ClientFrame,
    ) -> AppResult<()> {
        use race::ClientFrame as F;
        let now = Instant::now();
        let pid = self.socks.get(&sock).and_then(|s| s.pid);
        let mut fx = Fx::default();
        let player = |pid: Option<usize>| pid.ok_or(AppError::Forbidden);
        match frame {
            F::Next | F::End => {
                if role != Role::Host || self.quest.as_ref().is_some_and(|q| q.host != user) {
                    return Err(AppError::Forbidden);
                }
                self.quest_touch();
                let wall = self.wall(now);
                let q = self.quest.as_mut().expect("a quest game");
                match (frame, q.phase) {
                    (F::End, _) | (_, QPhase::Podium) => {
                        self.quest_finish(EndReason::Host).await;
                        return Ok(());
                    }
                    (_, QPhase::Lobby) => q.begin(now, wall, &mut fx),
                    (_, QPhase::Running { .. }) => {
                        return Err(AppError::Conflict("the game is running".into()));
                    }
                }
            }
            F::Character(c) => {
                let pid = player(pid)?;
                let q = self.quest.as_mut().expect("a quest game");
                q.character(pid, c)?;
            }
            F::QuestAnswer { serial, mask } => {
                let pid = player(pid)?;
                self.quest_touch();
                metrics::counter!("quiz_answers_total").increment(1);
                let q = self.quest.as_mut().expect("a quest game");
                q.answer(pid, serial, mask, now, &mut fx)?;
            }
            F::QuestPick { serial, chest } => {
                let pid = player(pid)?;
                self.quest_touch();
                let q = self.quest.as_mut().expect("a quest game");
                q.pick(pid, serial, chest, now, &mut fx)?;
            }
            F::QuestNext => {
                let pid = player(pid)?;
                let q = self.quest.as_mut().expect("a quest game");
                q.ask_again(pid, now, &mut fx)?;
            }
            _ => return Err(AppError::Conflict("not a frame of this game".into())),
        }
        self.apply_fx(fx);
        self.schedule_flush();
        Ok(())
    }

    pub(super) fn quest_next_wake(&self) -> Option<Instant> {
        let q = self.quest.as_ref()?;
        // the spans' heartbeat also runs while the quest waits in the lobby
        // or on the podium
        q.next_wake().min(Some(self.spans.next_tick))
    }

    pub(super) async fn quest_on_timer(&mut self) {
        let now = Instant::now();
        let Some(q) = self.quest.as_mut() else { return };
        let mut fx = Fx::default();
        q.tick(now, &mut fx);
        if let QPhase::Running { ends_at } = q.phase {
            if now >= ends_at {
                q.time_up(&mut fx);
            } else if now >= q.next_standings {
                q.standings(now, &mut fx);
            }
        }
        let (rc_at, idle_at) = (q.rc_at, q.idle_at);
        self.apply_fx(fx);
        self.schedule_flush();
        if now >= rc_at {
            self.quest_finish(EndReason::RoomClosed).await;
        } else if now >= idle_at {
            self.quest_finish(EndReason::Idle).await;
        }
        if self.quest.is_some() && now >= self.spans.next_tick {
            let due = self.spans.tick(self.wall(now), now);
            self.write_spans(due).await;
        }
    }

    /// The throttled host frames: the board and the roster.
    pub(super) fn quest_flush(&mut self) {
        let now = Instant::now();
        self.flush_at = None;
        self.last_flush = now;
        self.count_dirty = false;
        let Some(q) = self.quest.as_mut() else { return };
        let roster = std::mem::take(&mut q.roster_new);
        let board = q.board(now);
        let lobby_count = (q.phase == QPhase::Lobby).then_some(q.active_count);
        if let Some(n) = lobby_count {
            self.broadcast_shared(race::count(n));
        }
        if !roster.is_empty() {
            let refs: Vec<(&str, u8)> = roster.iter().map(|(n, c)| (n.as_str(), *c)).collect();
            self.broadcast_hosts(frames::roster(&refs));
        }
        if let Some(b) = board {
            self.broadcast_hosts(b);
        }
    }

    /// End the game: store the results in one transaction, close the spans,
    /// tell everyone.
    pub(super) async fn quest_finish(&mut self, reason: EndReason) {
        let Some(q) = self.quest.take() else { return };
        let ended_at = self.now_wall();
        let players: Vec<PlayerWrite> = q
            .players
            .iter()
            .filter(|p| p.active)
            .map(|p| PlayerWrite {
                id: p.id,
                user_id: p.user.clone(),
                name: p.name.clone(),
                joined_at: p.joined_at,
                score: p.coins,
                total_ms: 0,
                correct_count: i64::from(p.correct),
                answered: i64::from(p.answered),
                best_streak: i64::from(p.best_streak),
            })
            .collect();
        let batch = Batch {
            game_id: q.id,
            players,
            answers: Vec::new(),
            ended_at: Some(ended_at),
        };
        if let Err(e) = self.deps.store.write(batch).await {
            tracing::error!(error = %e, game = q.id, "writing the quest results failed");
        }
        let closed = self.spans.close_all(ended_at);
        self.write_spans(closed).await;
        for s in self.socks.values_mut() {
            s.pid = None;
        }
        self.deps.rooms.game_ended(&self.sid);
        self.broadcast_shared(race::ended(reason));
    }
}
