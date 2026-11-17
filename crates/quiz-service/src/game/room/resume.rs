//! A room's side of surviving a restart: what of a game is written down, when,
//! and how a written game is put back (see [`crate::game::resume`]).

use super::{DISCONNECT_GRACE, Game, Phase, Player, Room};
use crate::game::resume::{RECONNECT_ALLOWANCE, SAVE_EVERY, Saved, SavedGame, Writer};
use crate::model::QuestionWithOptions;
use crate::repo::games::AnswerWrite;
use axum::extract::ws::Utf8Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// A race game as it is stored. Deadlines are wall-clock times (unix ms).
#[derive(Serialize, Deserialize)]
pub struct RaceSnap {
    id: i64,
    title: String,
    host: String,
    questions: Arc<Vec<QuestionWithOptions>>,
    phase: Phase,
    qi: usize,
    q_started: i64,
    q_deadline: i64,
    reveal_at: i64,
    rc_at: i64,
    players: Vec<Player>,
    standings: Vec<usize>,
    pending: Vec<AnswerWrite>,
    reveal: Option<String>,
    podium: Option<(String, String)>,
    stored: bool,
}

impl RaceSnap {
    pub fn id(&self) -> i64 {
        self.id
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Why this cannot be played on, when it cannot.
    pub fn check(&self) -> Result<(), &'static str> {
        if self.qi >= self.questions.len() {
            return Err("the question index is past the questions");
        }
        if self.standings.iter().any(|&i| i >= self.players.len()) {
            return Err("a standing names a player who is not there");
        }
        Ok(())
    }
}

impl Room {
    /// Where the game of this room is, for telling a phase change (written at
    /// once) from a change within a phase (written within [`SAVE_EVERY`]).
    fn state_key(&self) -> Option<(u8, usize)> {
        if let Some(q) = &self.quest {
            return Some(q.resume_key());
        }
        self.game.as_ref().map(|g| (g.phase as u8, g.qi))
    }

    /// The state of the room's game, or `None` without one.
    fn game_state(&self) -> Option<SavedGame> {
        if let Some(q) = &self.quest {
            return Some(SavedGame::Quest(q.snap(|at| self.wall(at))));
        }
        let g = self.game.as_ref()?;
        let text = |b: &Utf8Bytes| b.to_string();
        Some(SavedGame::Race(RaceSnap {
            id: g.id,
            title: g.title.clone(),
            host: g.host.clone(),
            questions: g.questions.clone(),
            phase: g.phase,
            qi: g.qi,
            q_started: self.wall(g.q_started),
            q_deadline: self.wall(g.q_deadline),
            reveal_at: self.wall(g.reveal_at),
            rc_at: self.wall(g.rc_at),
            players: g.players.clone(),
            standings: g.standings.clone(),
            pending: g.pending.clone(),
            reveal: g.reveal.as_ref().map(text),
            podium: g.podium.as_ref().map(|(p, h)| (text(p), text(h))),
            stored: g.stored,
        }))
    }

    /// The state changed within a phase: it is written within [`SAVE_EVERY`].
    pub(in crate::game) fn mark_state_dirty(&mut self) {
        if self.snap_due.is_none() && self.state_key().is_some() {
            let due = self.snap_last + SAVE_EVERY;
            self.snap_due = Some(due.max(Instant::now()));
        }
    }

    /// After every step of the room: hand the game's state to the writer when
    /// its phase changed or it is due, and its deletion when the game ended.
    pub(super) fn sync_resume(&mut self) {
        let key = self.state_key();
        if key != self.snap_key {
            self.snap_key = key;
            match key {
                Some(_) => self.save_state(false),
                None => self.forget_state(),
            }
        } else if self.snap_due.is_some_and(|t| Instant::now() >= t) {
            self.save_state(false);
        }
    }

    fn writer(&mut self) -> &mut Writer {
        let store = self.deps.store.clone();
        self.writer.get_or_insert_with(|| Writer::start(store))
    }

    /// Hand the game's state to the writer; `paused` when the service is stopping.
    fn save_state(&mut self, paused: bool) {
        self.snap_due = None;
        self.snap_last = Instant::now();
        let (Some(game), Some(quiz_id)) = (self.game_state(), self.deps.rooms.quiz_of(&self.sid))
        else {
            return;
        };
        self.snap_game = Some(game.id());
        let saved = Saved {
            quiz_id,
            paused_at: paused.then(|| self.now_wall()),
            game,
        };
        match saved.row(&self.sid, (self.deps.now_ms)()) {
            Ok(row) => self.writer().save(row),
            Err(e) => {
                tracing::error!(error = %e, room = %self.sid, "encoding a running game failed")
            }
        }
    }

    fn forget_state(&mut self) {
        self.snap_due = None;
        let sid = self.sid.clone();
        if let Some(game_id) = self.snap_game.take() {
            self.writer().delete(sid, game_id);
        }
    }

    /// The service stops: write the state of the game as it is now, marked as paused (so
    /// that the time until the next process is not taken from the players), and wait for
    /// the database to have it. The game is not ended.
    pub(super) async fn save_final(&mut self) {
        if self.state_key().is_some() {
            self.save_state(true);
        }
        if let Some(w) = &self.writer {
            w.flush().await;
        }
    }

    /// Put a stored game back, with nobody attached. Whoever was connected is gone, so each
    /// player has the grace of a dropped connection to come back before the open question
    /// stops waiting for them.
    ///
    /// `paused` is `Some(ms)` after a planned stop: the game stood still for `ms`, so its
    /// clocks run on from where they stopped; its deadlines get a reconnect allowance on top
    /// and the grace ends that long after the restore (see [`RECONNECT_ALLOWANCE`]). After a crash the clocks are wall-clock. The room's
    /// close time is the lesson's and does not move. The state is stored again at once, no
    /// longer marked paused: a crash from now on must not push the clocks by time already played.
    pub(in crate::game) fn resume(&mut self, game: SavedGame, paused: Option<i64>) {
        self.snap_game = Some(game.id());
        let downtime = paused.unwrap_or(0);
        let allowance = paused.map_or(0, |_| RECONNECT_ALLOWANCE.as_millis() as i64);
        match game {
            SavedGame::Race(s) => self.resume_race(s, downtime, allowance),
            SavedGame::Quest(s) => self.resume_quest(s, downtime, allowance),
        }
        self.snap_key = self.state_key();
        self.save_state(false);
    }

    fn resume_race(&mut self, s: RaceSnap, downtime: i64, allowance: i64) {
        let now = Instant::now();
        let gone_at =
            now + Duration::from_millis(allowance as u64).saturating_sub(DISCONNECT_GRACE);
        let lobby = s.phase == Phase::Lobby;
        let mut players = s.players;
        for p in &mut players {
            p.dirty = true;
            p.gone_at = Some(gone_at);
            // a lobby is made of whoever is there: they come back by joining
            p.active &= !lobby;
        }
        let by_user: HashMap<String, usize> = players
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.user.clone().map(|u| (u, i)))
            .collect();
        let question = &s.questions[s.qi];
        let mut choices = Vec::new();
        if s.phase == Phase::Question {
            choices = vec![0; question.options.len()];
            for a in players.iter().filter_map(|p| p.answer.as_ref()) {
                for (bit, c) in choices.iter_mut().enumerate() {
                    *c += a.mask >> bit & 1;
                }
            }
        }
        let game = Game {
            id: s.id,
            title: s.title,
            host: s.host,
            questions: s.questions,
            phase: s.phase,
            qi: s.qi,
            q_started: self.instant_at(s.q_started + downtime),
            q_deadline: self.instant_at(s.q_deadline + downtime + allowance),
            reveal_at: self.instant_at(s.reveal_at + downtime + allowance),
            active_count: players.iter().filter(|p| p.active).count(),
            answered: players.iter().filter(|p| p.answer.is_some()).count(),
            players,
            by_user,
            choices,
            new_names: Vec::new(),
            standings: s.standings,
            pending: s.pending,
            rc_at: self.instant_at(s.rc_at),
            idle_at: now + super::IDLE_LIMIT,
            reveal: s.reveal.map(Utf8Bytes::from),
            stored: s.stored,
            podium: s
                .podium
                .map(|(p, h)| (Utf8Bytes::from(p), Utf8Bytes::from(h))),
        };
        self.game = Some(game);
    }
}
