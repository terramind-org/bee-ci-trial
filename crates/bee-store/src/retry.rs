//! Transaction retry rules.
//!
//! Store implementations retry their own serialization failures with [`retry`] under
//! [`RetryPolicy::TRANSACTION`] (10 attempts, full-jitter exponential backoff) and surface
//! `UNAVAILABLE` (retryable) when the attempts run out. Callers retry only errors whose
//! `retryable` flag is set (SPEC §12.2, §15.5). Jitter comes from a caller-supplied seed, so
//! a run is reproducible.

use std::future::Future;
use std::time::Duration;

use bee_core::Result;

/// How many attempts and how long to wait between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, the first included.
    pub max_attempts: u32,
    /// Backoff cap before jitter for the first retry; doubles per retry.
    pub base_delay: Duration,
    /// Largest backoff cap.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// The rule for store transactions: 10 attempts, 2 ms doubling up to 200 ms.
    pub const TRANSACTION: RetryPolicy = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_millis(2),
        max_delay: Duration::from_millis(200),
    };

    /// The full-jitter delay before retry number `retry` (1-based): uniform in
    /// `[0, min(max_delay, base_delay * 2^(retry-1))]`, drawn deterministically from `seed`.
    pub fn delay(&self, retry: u32, seed: u64) -> Duration {
        let cap = self
            .base_delay
            .saturating_mul(1u32 << retry.saturating_sub(1).min(20))
            .min(self.max_delay);
        let cap_us = cap.as_micros() as u64;
        if cap_us == 0 {
            return Duration::ZERO;
        }
        let r = splitmix64(seed ^ u64::from(retry).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        Duration::from_micros(r % (cap_us + 1))
    }
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Runs `op(attempt)` (0-based) until it succeeds, fails with a non-retryable error, or the
/// policy's attempts run out (the last error is returned).
pub async fn retry<T, F, Fut>(policy: &RetryPolicy, seed: u64, mut op: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0;
    loop {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) if !e.retryable || attempt + 1 >= policy.max_attempts => return Err(e),
            Err(_) => {
                attempt += 1;
                tokio::time::sleep(policy.delay(attempt, seed)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::{BeeError, Reason};

    #[test]
    fn delays_are_bounded_and_deterministic() {
        let p = RetryPolicy::TRANSACTION;
        for retry in 1..12 {
            let d = p.delay(retry, 42);
            assert_eq!(d, p.delay(retry, 42));
            assert!(d <= p.max_delay);
            assert!(d <= p.base_delay * (1 << (retry - 1).min(20)));
        }
        let none = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        };
        assert_eq!(none.delay(3, 1), Duration::ZERO);
    }

    #[tokio::test]
    async fn retries_only_retryable_errors_up_to_the_budget() {
        let p = RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_micros(10),
            max_delay: Duration::from_micros(50),
        };
        let mut calls = 0;
        let r: Result<u32> = retry(&p, 7, |_| {
            calls += 1;
            async { Err(BeeError::new(Reason::Unavailable, "busy")) }
        })
        .await;
        assert_eq!(r.unwrap_err().reason, Reason::Unavailable);
        assert_eq!(calls, 4);

        let mut calls = 0;
        let r: Result<u32> = retry(&p, 7, |_| {
            calls += 1;
            async { Err(BeeError::new(Reason::NotFound, "gone")) }
        })
        .await;
        assert_eq!(r.unwrap_err().reason, Reason::NotFound);
        assert_eq!(calls, 1);

        let r = retry(&p, 7, |attempt| async move {
            if attempt < 2 {
                Err(BeeError::new(Reason::Unavailable, "busy"))
            } else {
                Ok(attempt)
            }
        })
        .await;
        assert_eq!(r.unwrap(), 2);
    }
}
