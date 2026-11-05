//! Random primary key generation.
//!
//! The begin-concurrent branch relies on optimistic page-level locking:
//! transactions conflict only when they touch the same database pages.
//! Monotonically increasing rowids funnel all concurrent inserts onto the
//! same (rightmost) b-tree page and cause SQLITE_BUSY_SNAPSHOT storms.
//! Random 62-bit ids spread inserts across the whole key space, which makes
//! concurrent `BEGIN CONCURRENT` commits actually concurrent.
//! (see third_party/sqlite/doc/begin_concurrent.md)

/// Generate a fresh random 62-bit positive integer id (never 0).
pub fn new_id() -> i64 {
    loop {
        // Mask to 62 bits: stays positive in i64 and far away from the
        // rowid "max row" trick region SQLite uses internally.
        let id = rand::random::<i64>() & 0x3FFF_FFFF_FFFF_FFFF;
        if id != 0 {
            return id;
        }
    }
}

/// Current unix time in milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_positive_and_unique_enough() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let id = new_id();
            assert!(id > 0);
            assert!(id < (1 << 62));
            assert!(
                seen.insert(id),
                "collision within 10k ids (astronomically unlikely)"
            );
        }
    }
}
