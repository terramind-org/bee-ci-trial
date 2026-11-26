//! Small shared helpers: fresh identifiers, randomness and time conversions.
//!
//! Randomness and the wall clock are confined to this module and the injected
//! [`bee_store::Clock`]; nothing that computes an object id reads either.

use bee_core::{BeeError, Reason, Result, Ulid};

/// Fills `n` bytes from the operating system's random source.
pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; n];
    getrandom::fill(&mut out)
        .map_err(|e| BeeError::new(Reason::Internal, format!("random source failed: {e}")))?;
    Ok(out)
}

/// A fresh ULID at `now_ms` with 80 random bits.
pub fn new_ulid(now_ms: u64) -> Result<Ulid> {
    let r = random_bytes(10)?;
    let mut random: u128 = 0;
    for b in r {
        random = (random << 8) | u128::from(b);
    }
    Ulid::from_parts(now_ms, random)
}

/// A random lowercase-hex identifier of `bytes` bytes (request ids, import ids).
pub fn random_hex(bytes: usize) -> String {
    random_bytes(bytes)
        .map(|b| bee_core::hex::encode(&b))
        .unwrap_or_else(|_| "0".repeat(bytes * 2))
}

/// A protobuf timestamp for Unix milliseconds.
pub fn timestamp_ms(ms: u64) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: (ms / 1000) as i64,
        nanos: ((ms % 1000) * 1_000_000) as i32,
    }
}

/// A protobuf timestamp for Unix seconds.
pub fn timestamp_secs(secs: u64) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: secs as i64,
        nanos: 0,
    }
}

/// Unix seconds of a protobuf timestamp (negative times clamp to 0).
pub fn ts_secs(ts: &pbjson_types::Timestamp) -> u64 {
    ts.seconds.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulids_are_fresh_and_time_ordered() {
        let a = new_ulid(1_000).unwrap();
        let b = new_ulid(1_000).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.timestamp_ms(), 1_000);
        assert!(new_ulid(2_000).unwrap() > a);
        assert_eq!(random_hex(8).len(), 16);
    }

    #[test]
    fn timestamps_round_trip() {
        let t = timestamp_ms(1_500);
        assert_eq!((t.seconds, t.nanos), (1, 500_000_000));
        assert_eq!(ts_secs(&timestamp_secs(42)), 42);
    }
}
