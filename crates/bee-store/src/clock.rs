//! An injected clock. Store code never reads wall-clock time itself; it asks a [`Clock`].

use std::fmt::Debug;
use std::sync::atomic::{AtomicU64, Ordering};

/// A source of "now" in milliseconds since the Unix epoch.
pub trait Clock: Send + Sync + Debug {
    /// The current time.
    fn now_ms(&self) -> u64;
}

/// The operating system's clock, for production stores.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A clock that only moves when told to.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    /// A clock reading `now_ms`.
    pub fn new(now_ms: u64) -> ManualClock {
        ManualClock {
            now: AtomicU64::new(now_ms),
        }
    }

    /// Sets the time.
    pub fn set(&self, now_ms: u64) {
        self.now.store(now_ms, Ordering::SeqCst);
    }

    /// Moves the time forward and returns the new time.
    pub fn advance(&self, ms: u64) -> u64 {
        self.now.fetch_add(ms, Ordering::SeqCst) + ms
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_told() {
        let c = ManualClock::new(5);
        assert_eq!(c.now_ms(), 5);
        assert_eq!(c.advance(10), 15);
        c.set(3);
        assert_eq!(c.now_ms(), 3);
        assert!(SystemClock.now_ms() > 1_600_000_000_000);
    }
}
