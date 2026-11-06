//! Connected stretches of the people in a running game.
//!
//! A person with at least one socket attached has an open span. When the
//! last socket goes the span waits for [`BRIDGE`]: a reconnect within it
//! extends the same span (the short gap counts, as it does for the call),
//! otherwise the span is closed at the moment the person left. Every open
//! span (the host's and each connected player's) is written to the outbox
//! every [`HEARTBEAT_EVERY`] with its end so far, so a crash of the service, or
//! a Go server that cannot be reached at the end of the lesson, costs at most
//! that minute of attendance: the Go server keeps the largest end it was told
//! and a replay is idempotent.

use crate::auth::Role;
use crate::repo::spans::Span;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

/// A reconnect within this long extends the span instead of opening one.
pub const BRIDGE: Duration = Duration::from_secs(60);
/// How often the open spans are rewritten with their end so far.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(60);
/// How often the book looks for work.
const TICK: Duration = Duration::from_secs(15);

struct Open {
    role: Role,
    from: i64,
    /// Last moment the person was known to be there.
    to: i64,
    live: u32,
    left_at: Option<Instant>,
}

pub struct SpanBook {
    game_id: i64,
    session_id: String,
    enabled: bool,
    open: HashMap<String, Open>,
    pub next_tick: Instant,
    heartbeat_written: Instant,
}

impl Default for SpanBook {
    fn default() -> Self {
        let now = Instant::now();
        SpanBook {
            game_id: 0,
            session_id: String::new(),
            enabled: false,
            open: HashMap::new(),
            next_tick: now,
            heartbeat_written: now,
        }
    }
}

impl SpanBook {
    /// Start tracking a game.
    pub fn begin(&mut self, game_id: i64, session_id: &str, enabled: bool, now: Instant) {
        *self = SpanBook {
            game_id,
            session_id: session_id.to_string(),
            enabled,
            open: HashMap::new(),
            next_tick: now + TICK,
            heartbeat_written: now,
        };
    }

    pub fn attach(&mut self, user: &str, role: Role, wall: i64) {
        if !self.enabled {
            return;
        }
        let o = self.open.entry(user.to_string()).or_insert(Open {
            role,
            from: wall,
            to: wall,
            live: 0,
            left_at: None,
        });
        o.live += 1;
        o.left_at = None;
    }

    pub fn detach(&mut self, user: &str, wall: i64, now: Instant) {
        if let Some(o) = self.open.get_mut(user) {
            o.live = o.live.saturating_sub(1);
            if o.live == 0 {
                o.to = wall;
                o.left_at = Some(now);
            }
        }
    }

    fn span(&self, user: &str, o: &Open, to: i64, closed: bool) -> Span {
        Span {
            game_id: self.game_id,
            user_id: user.to_string(),
            session_id: self.session_id.clone(),
            role: if o.role == Role::Host {
                "host"
            } else {
                "player"
            }
            .to_string(),
            from_ms: o.from,
            to_ms: to,
            closed,
        }
    }

    /// Spans to write now: those whose bridge ran out, closed at the moment
    /// the person left, and every connected person's open span when the minute
    /// is up.
    pub fn tick(&mut self, wall: i64, now: Instant) -> Vec<Span> {
        self.next_tick = now + TICK;
        let mut out = Vec::new();
        let heartbeat = now.saturating_duration_since(self.heartbeat_written) >= HEARTBEAT_EVERY;
        let expired: Vec<String> = self
            .open
            .iter()
            .filter(|(_, o)| {
                o.left_at
                    .is_some_and(|l| now.saturating_duration_since(l) >= BRIDGE)
            })
            .map(|(u, _)| u.clone())
            .collect();
        for user in expired {
            if let Some(o) = self.open.remove(&user) {
                out.push(self.span(&user, &o, o.to, true));
            }
        }
        if heartbeat {
            self.heartbeat_written = now;
            for (user, o) in &self.open {
                if o.live > 0 {
                    out.push(self.span(user, o, wall, false));
                }
            }
        }
        out
    }

    /// Close everything, as of `wall` for those still connected.
    pub fn close_all(&mut self, wall: i64) -> Vec<Span> {
        let open = std::mem::take(&mut self.open);
        open.iter()
            .map(|(user, o)| self.span(user, o, if o.live > 0 { wall } else { o.to }, true))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(now: Instant) -> SpanBook {
        let mut b = SpanBook::default();
        b.begin(7, "s1", true, now);
        b
    }

    #[tokio::test(start_paused = true)]
    async fn a_span_closes_at_the_moment_the_person_left() {
        let t0 = Instant::now();
        let mut b = book(t0);
        b.attach("p1", Role::Player, 1_000);
        tokio::time::advance(Duration::from_secs(30)).await;
        b.detach("p1", 31_000, Instant::now());
        tokio::time::advance(Duration::from_secs(61)).await;
        let spans = b.tick(92_000, Instant::now());
        assert_eq!(spans.len(), 1);
        assert_eq!(
            (spans[0].from_ms, spans[0].to_ms, spans[0].closed),
            (1_000, 31_000, true)
        );
        assert_eq!(spans[0].role, "player");
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_within_the_bridge_extends_the_span() {
        let mut b = book(Instant::now());
        b.attach("p1", Role::Player, 1_000);
        b.detach("p1", 11_000, Instant::now());
        tokio::time::advance(Duration::from_secs(30)).await;
        b.attach("p1", Role::Player, 41_000);
        tokio::time::advance(Duration::from_secs(70)).await;
        assert!(
            b.tick(111_000, Instant::now()).iter().all(|s| !s.closed),
            "still connected: nothing is closed"
        );
        b.detach("p1", 120_000, Instant::now());
        let closed = b.close_all(120_000);
        assert_eq!(closed.len(), 1);
        assert_eq!((closed[0].from_ms, closed[0].to_ms), (1_000, 120_000));
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_span_is_rewritten_every_minute() {
        let mut b = book(Instant::now());
        b.attach("h", Role::Host, 0);
        tokio::time::advance(Duration::from_secs(61)).await;
        let spans = b.tick(61_000, Instant::now());
        assert_eq!(spans.len(), 1);
        assert_eq!(
            (spans[0].role.as_str(), spans[0].to_ms, spans[0].closed),
            ("host", 61_000, false)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn players_are_rewritten_every_minute_too_while_connected() {
        let mut b = book(Instant::now());
        b.attach("p1", Role::Player, 0);
        b.attach("p2", Role::Player, 0);
        tokio::time::advance(Duration::from_secs(20)).await;
        b.detach("p2", 20_000, Instant::now());
        tokio::time::advance(Duration::from_secs(41)).await;
        let spans = b.tick(61_000, Instant::now());
        let p1: Vec<_> = spans.iter().filter(|s| s.user_id == "p1").collect();
        assert_eq!(p1.len(), 1, "{spans:?}");
        assert_eq!(
            (p1[0].role.as_str(), p1[0].to_ms, p1[0].closed),
            ("player", 61_000, false)
        );
        assert!(
            spans.iter().all(|s| s.user_id != "p2"),
            "gone for 41 s: still bridged, not written"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tracking_can_be_off() {
        let mut b = SpanBook::default();
        b.begin(1, "s", false, Instant::now());
        b.attach("p", Role::Player, 0);
        assert!(b.close_all(5).is_empty());
    }
}
