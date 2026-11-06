//! A token bucket per key, for the bank's writes: a ticket (a person) may
//! save, create, keep and delete quizzes at a human pace, not in a loop.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Above this many buckets, idle ones are forgotten.
const PRUNE_ABOVE: usize = 4096;
/// A bucket idle this long is full again and not worth remembering.
const IDLE: Duration = Duration::from_secs(15 * 60);

struct Bucket {
    tokens: f64,
    at: Instant,
}

/// `burst` requests at once, then `per_minute` a minute.
pub struct Limiter {
    burst: f64,
    per_second: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Limiter {
    pub fn new(burst: u32, per_minute: u32) -> Self {
        Limiter {
            burst: f64::from(burst.max(1)),
            per_second: f64::from(per_minute.max(1)) / 60.0,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one token for `key` at `now`; when there is none, the seconds
    /// to wait (at least 1) until there is.
    pub fn take(&self, key: &str, now: Instant) -> Result<(), u64> {
        let mut buckets = self.buckets.lock().expect("limiter lock");
        if buckets.len() > PRUNE_ABOVE {
            buckets.retain(|_, b| now.saturating_duration_since(b.at) < IDLE);
        }
        let b = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: self.burst,
            at: now,
        });
        let elapsed = now.saturating_duration_since(b.at).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.per_second).min(self.burst);
        b.at = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / self.per_second).ceil().max(1.0) as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_then_the_steady_rate() {
        let l = Limiter::new(3, 6); // one every 10 s
        let t0 = Instant::now();
        assert!((0..3).all(|_| l.take("a", t0).is_ok()), "the burst");
        assert_eq!(l.take("a", t0), Err(10), "then wait for the next token");
        assert!(l.take("b", t0).is_ok(), "somebody else is not affected");
        assert_eq!(l.take("a", t0 + Duration::from_secs(4)), Err(6));
        assert!(l.take("a", t0 + Duration::from_secs(10)).is_ok());
        assert!(l.take("a", t0 + Duration::from_secs(10)).is_err());
        // a long pause refills to the burst, not beyond
        let later = t0 + Duration::from_secs(3600);
        assert!((0..3).all(|_| l.take("a", later).is_ok()));
        assert!(l.take("a", later).is_err());
    }

    #[test]
    fn idle_buckets_are_forgotten_when_there_are_many() {
        let l = Limiter::new(1, 1);
        let t0 = Instant::now();
        for i in 0..=PRUNE_ABOVE {
            l.take(&format!("k{i}"), t0).unwrap();
        }
        l.take("fresh", t0 + IDLE + Duration::from_secs(1)).unwrap();
        assert!(l.buckets.lock().unwrap().len() < 10);
    }
}
