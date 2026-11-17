//! A quest game's side of surviving a restart: its state as it is stored and
//! put back (see [`crate::game::resume`]). Deadlines are wall-clock times
//! (unix ms), so the time the service was down counts.

use super::chest::{Draw, Outcome};
use super::{PState, QPhase, QPlayer, Quest, STANDINGS_EVERY};
use crate::game::room::{IDLE_LIMIT, Room};
use crate::model::QuestionWithOptions;
use axum::extract::ws::Utf8Bytes;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::sync::Arc;
use tokio::time::Instant;

#[derive(Serialize, Deserialize)]
enum PhaseSnap {
    Lobby,
    Running { ends_at: i64 },
    Podium,
}

#[derive(Serialize, Deserialize)]
enum StateSnap {
    Idle,
    Asking {
        q: usize,
        deadline: i64,
        served: i64,
    },
    Chest([Outcome; 3]),
    Pause(i64),
}

#[derive(Serialize, Deserialize)]
struct PlayerSnap {
    id: i64,
    user: Option<String>,
    name: String,
    joined_at: i64,
    character: u8,
    coins: i64,
    streak: u32,
    best_streak: u32,
    booster: u8,
    shield: bool,
    queue: Vec<u16>,
    qpos: usize,
    serial: u32,
    state: StateSnap,
    answered: u32,
    correct: u32,
    active: bool,
    rank: u32,
}

/// A quest game as it is stored.
#[derive(Serialize, Deserialize)]
pub struct QuestSnap {
    id: i64,
    title: String,
    host: String,
    questions: Arc<Vec<QuestionWithOptions>>,
    seconds: u32,
    friendly: bool,
    phase: PhaseSnap,
    players: Vec<PlayerSnap>,
    answers: u64,
    correct: u64,
    podium: Option<(String, String)>,
    rc_at: i64,
    stored: bool,
}

impl QuestSnap {
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Why this cannot be played on, when it cannot.
    pub fn check(&self) -> Result<(), &'static str> {
        let n = self.questions.len();
        if n == 0 {
            return Err("the quiz has no questions");
        }
        for p in &self.players {
            if p.qpos > p.queue.len() || p.queue.iter().any(|&q| usize::from(q) >= n) {
                return Err("a player's queue names a question that is not there");
            }
            if matches!(p.state, StateSnap::Asking { q, .. } if q >= n) {
                return Err("a player is asked a question that is not there");
            }
        }
        Ok(())
    }
}

impl QPlayer {
    fn snap(&self, wall: &impl Fn(Instant) -> i64) -> PlayerSnap {
        PlayerSnap {
            id: self.id,
            user: self.user.clone(),
            name: self.name.clone(),
            joined_at: self.joined_at,
            character: self.character,
            coins: self.coins,
            streak: self.streak,
            best_streak: self.best_streak,
            booster: self.booster,
            shield: self.shield,
            queue: self.queue.clone(),
            qpos: self.qpos,
            serial: self.serial,
            state: match self.state {
                PState::Idle => StateSnap::Idle,
                PState::Asking {
                    q,
                    deadline,
                    served,
                } => StateSnap::Asking {
                    q,
                    deadline: wall(deadline),
                    served: wall(served),
                },
                PState::Chest(outcomes) => StateSnap::Chest(outcomes),
                PState::Pause(until) => StateSnap::Pause(wall(until)),
            },
            answered: self.answered,
            correct: self.correct,
            active: self.active,
            rank: self.rank,
        }
    }

    fn from_snap(
        s: PlayerSnap,
        at: &impl Fn(i64) -> Instant,
        deadline_at: &impl Fn(i64) -> Instant,
    ) -> QPlayer {
        QPlayer {
            id: s.id,
            user: s.user,
            name: s.name,
            joined_at: s.joined_at,
            character: s.character,
            coins: s.coins,
            streak: s.streak,
            best_streak: s.best_streak,
            booster: s.booster,
            shield: s.shield,
            queue: s.queue,
            qpos: s.qpos,
            serial: s.serial,
            state: match s.state {
                StateSnap::Idle => PState::Idle,
                StateSnap::Asking {
                    q,
                    deadline,
                    served,
                } => PState::Asking {
                    q,
                    deadline: deadline_at(deadline),
                    served: at(served),
                },
                StateSnap::Chest(outcomes) => PState::Chest(outcomes),
                StateSnap::Pause(until) => PState::Pause(deadline_at(until)),
            },
            answered: s.answered,
            correct: s.correct,
            socks: Vec::new(),
            active: s.active,
            rank: s.rank,
            sent_rank: 0,
            sent_coins: i64::MIN,
        }
    }
}

impl Quest {
    /// Where the game is, for telling a phase change from a change within one.
    pub(in crate::game) fn resume_key(&self) -> (u8, usize) {
        let phase = match self.phase {
            QPhase::Lobby => 0,
            QPhase::Running { .. } => 1,
            QPhase::Podium => 2,
        };
        (phase, 0)
    }

    /// The state to store; `wall` turns an instant into unix ms.
    pub(in crate::game) fn snap(&self, wall: impl Fn(Instant) -> i64) -> QuestSnap {
        let text = |b: &Utf8Bytes| b.to_string();
        QuestSnap {
            id: self.id,
            title: self.title.clone(),
            host: self.host.clone(),
            questions: self.questions.clone(),
            seconds: self.seconds,
            friendly: self.friendly,
            phase: match self.phase {
                QPhase::Lobby => PhaseSnap::Lobby,
                QPhase::Running { ends_at } => PhaseSnap::Running {
                    ends_at: wall(ends_at),
                },
                QPhase::Podium => PhaseSnap::Podium,
            },
            players: self.players.iter().map(|p| p.snap(&wall)).collect(),
            answers: self.answers,
            correct: self.correct,
            podium: self.podium.as_ref().map(|(p, h)| (text(p), text(h))),
            rc_at: wall(self.rc_at),
            stored: self.stored,
        }
    }

    /// A stored game with nobody attached. In a lobby nobody counts until
    /// they come back and join again.
    fn from_snap(
        s: QuestSnap,
        rng: Box<dyn Draw>,
        at: impl Fn(i64) -> Instant,
        downtime: i64,
        allowance: i64,
        now: Instant,
    ) -> Quest {
        // the clocks of the game stood still for the downtime and the deadlines get the
        // allowance on top; the room's close time did not move
        let rc_at = at(s.rc_at);
        let deadline_at = |ms: i64| at(ms + downtime + allowance);
        let at = |ms: i64| at(ms + downtime);
        let lobby = matches!(s.phase, PhaseSnap::Lobby);
        let mut quest = Quest::new(
            s.id,
            s.title,
            s.host,
            s.questions,
            s.seconds,
            s.friendly,
            rng,
            rc_at,
            now,
        );
        quest.phase = match s.phase {
            PhaseSnap::Lobby => QPhase::Lobby,
            PhaseSnap::Running { ends_at } => QPhase::Running {
                ends_at: deadline_at(ends_at),
            },
            PhaseSnap::Podium => QPhase::Podium,
        };
        quest.players = s
            .players
            .into_iter()
            .map(|p| QPlayer::from_snap(p, &at, &deadline_at))
            .collect();
        for (pid, p) in quest.players.iter_mut().enumerate() {
            p.active &= !lobby;
            if let PState::Asking { deadline: t, .. } | PState::Pause(t) = p.state {
                quest.timers.push(Reverse((t, pid, p.serial)));
            }
            if let Some(user) = &p.user {
                quest.by_user.insert(user.clone(), pid);
            }
        }
        quest.active_count = quest.players.iter().filter(|p| p.active).count();
        quest.answers = s.answers;
        quest.correct = s.correct;
        quest.stored = s.stored;
        quest.podium = s
            .podium
            .map(|(p, h)| (Utf8Bytes::from(p), Utf8Bytes::from(h)));
        quest.idle_at = now + IDLE_LIMIT;
        quest.next_standings = now + STANDINGS_EVERY;
        quest.board_dirty = true;
        quest
    }
}

impl Room {
    pub(in crate::game) fn resume_quest(&mut self, s: QuestSnap, downtime: i64, allowance: i64) {
        let quest = Quest::from_snap(
            s,
            (self.deps.quest_draw)(),
            |ms| self.instant_at(ms),
            downtime,
            allowance,
            Instant::now(),
        );
        self.quest = Some(quest);
    }
}
