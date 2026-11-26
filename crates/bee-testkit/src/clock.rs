//! A deterministic clock.

use std::sync::atomic::{AtomicU64, Ordering};

use bee_store::{Clock, DAY_MS};

/// 2026-10-18T00:00:00Z, the default start of every test clock.
pub const TEST_EPOCH_MS: u64 = 1_792_281_600_000;

/// A clock that moves only when a test moves it. Implements [`bee_store::Clock`].
#[derive(Debug)]
pub struct TestClock {
    now: AtomicU64,
}

impl Default for TestClock {
    fn default() -> Self {
        TestClock::new(TEST_EPOCH_MS)
    }
}

impl TestClock {
    /// A clock reading `start_ms`.
    pub fn new(start_ms: u64) -> TestClock {
        TestClock {
            now: AtomicU64::new(start_ms),
        }
    }

    /// The current time in milliseconds.
    pub fn now(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    /// The current time in whole seconds.
    pub fn now_secs(&self) -> i64 {
        (self.now() / 1000) as i64
    }

    /// Moves forward by `ms` and returns the new time.
    pub fn advance_ms(&self, ms: u64) -> u64 {
        self.now.fetch_add(ms, Ordering::SeqCst) + ms
    }

    /// Moves forward by whole days.
    pub fn advance_days(&self, days: u64) -> u64 {
        self.advance_ms(days * DAY_MS)
    }

    /// Sets the time.
    pub fn set(&self, ms: u64) {
        self.now.store(ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_only_when_told() {
        let c = TestClock::default();
        assert_eq!(c.now(), TEST_EPOCH_MS);
        assert_eq!(c.now_ms(), TEST_EPOCH_MS);
        assert_eq!(c.advance_days(1), TEST_EPOCH_MS + DAY_MS);
        assert_eq!(c.advance_ms(5), TEST_EPOCH_MS + DAY_MS + 5);
        c.set(2000);
        assert_eq!(c.now_secs(), 2);
    }
}
