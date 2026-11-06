//! The service's wall clock. It reads the system time plus an offset that
//! only tests move, so expiry rules can be exercised without waiting.

use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Debug, Default)]
pub struct Clock {
    offset_ms: AtomicI64,
}

impl Clock {
    /// Current unix time in milliseconds.
    pub fn now_ms(&self) -> i64 {
        crate::ids::now_ms() + self.offset_ms.load(Ordering::Relaxed)
    }

    /// Move the clock forward (tests only).
    #[doc(hidden)]
    pub fn advance_ms(&self, ms: i64) {
        self.offset_ms.fetch_add(ms, Ordering::Relaxed);
    }
}
