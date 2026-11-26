//! ULID generation and time helpers.
//!
//! Forge ids are ULIDs (SPEC §3.2): 48 bits of milliseconds from the injected
//! [`Clock`] and 80 random bits. Within one generator, ids are strictly increasing even inside
//! one millisecond (the random part is incremented), so sorting by id sorts by creation.
//! Forge ids never feed a content hash, so they may use time and randomness.

use std::sync::{Arc, Mutex};

use bee_core::Ulid;
pub use bee_store::clock::{Clock, ManualClock, SystemClock};

use crate::error::{ForgeError, Result};

const RANDOM_MASK: u128 = (1 << 80) - 1;

/// A monotonic ULID generator over an injected clock.
#[derive(Debug)]
pub struct IdGen {
    clock: Arc<dyn Clock>,
    last: Mutex<(u64, u128)>,
}

impl IdGen {
    /// A generator reading `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> IdGen {
        IdGen {
            clock,
            last: Mutex::new((0, 0)),
        }
    }

    /// The clock.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Current time in milliseconds.
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// A new ULID in text form.
    pub fn ulid(&self) -> Result<String> {
        let now = self.clock.now_ms();
        let mut last = self
            .last
            .lock()
            .map_err(|_| ForgeError::internal("id generator poisoned"))?;
        let (ms, rand) = if now <= last.0 {
            // Same (or earlier) millisecond: keep the last timestamp and step the random part.
            let next = (last.1 + 1) & RANDOM_MASK;
            if next == 0 {
                (last.0 + 1, random80()?)
            } else {
                (last.0, next)
            }
        } else {
            (now, random80()?)
        };
        *last = (ms, rand);
        let id = Ulid::from_parts(ms, rand).map_err(|e| ForgeError::internal(e.message))?;
        Ok(id.to_text())
    }

    /// 32 random bytes, hex encoded (secrets such as delivery GUIDs).
    pub fn random_hex(&self, bytes: usize) -> Result<String> {
        let mut buf = vec![0u8; bytes];
        getrandom::fill(&mut buf).map_err(|e| ForgeError::internal(format!("rng: {e}")))?;
        Ok(hex(&buf))
    }
}

fn random80() -> Result<u128> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf[6..]).map_err(|e| ForgeError::internal(format!("rng: {e}")))?;
    // Leave one bit of headroom so a same-millisecond increment rarely overflows.
    Ok((u128::from_be_bytes(buf) & RANDOM_MASK) >> 1)
}

/// Lower-case hex of bytes.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parses lower- or upper-case hex.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A protobuf timestamp from milliseconds since the epoch.
pub fn ts(ms: u64) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: (ms / 1000) as i64,
        nanos: ((ms % 1000) * 1_000_000) as i32,
    }
}

/// Milliseconds since the epoch of a protobuf timestamp (0 for negative times).
pub fn ts_ms(t: &pbjson_types::Timestamp) -> u64 {
    if t.seconds < 0 {
        return 0;
    }
    (t.seconds as u64) * 1000 + (t.nanos.max(0) as u64) / 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_increase_within_a_millisecond() {
        let gen_ = IdGen::new(Arc::new(ManualClock::new(1_700_000_000_000)));
        let mut prev = gen_.ulid().unwrap();
        for _ in 0..1000 {
            let next = gen_.ulid().unwrap();
            assert!(next > prev, "{next} <= {prev}");
            assert_eq!(next.len(), 26);
            prev = next;
        }
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(
            unhex(&hex(&[0, 1, 0xab, 0xff])).unwrap(),
            vec![0, 1, 0xab, 0xff]
        );
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
        assert_eq!(ts_ms(&ts(1_234_567)), 1_234_567);
    }
}
