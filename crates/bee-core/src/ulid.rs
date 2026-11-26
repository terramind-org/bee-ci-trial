//! ULID surrogate ids (SPEC §3.2): 48-bit millisecond timestamp plus 80 random bits,
//! written as 26 characters of Crockford base32.
//!
//! This module never reads the clock or a random source; callers pass both in, so id
//! generation stays testable and deterministic under simulation.

use std::fmt;
use std::str::FromStr;

use crate::error::{BeeError, Result, Subcode};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Length of the text form.
pub const ULID_TEXT_LEN: usize = 26;
/// Largest timestamp a ULID can hold (2^48 - 1 milliseconds).
pub const MAX_TIMESTAMP_MS: u64 = (1 << 48) - 1;
/// Mask of the 80 random bits.
const RANDOM_MASK: u128 = (1 << 80) - 1;

/// A 128-bit ULID.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ulid(u128);

impl Ulid {
    /// Builds a ULID from a millisecond timestamp and 80 random bits.
    ///
    /// Fails if the timestamp exceeds 48 bits or `random` exceeds 80 bits.
    pub fn from_parts(timestamp_ms: u64, random: u128) -> Result<Ulid> {
        if timestamp_ms > MAX_TIMESTAMP_MS || random > RANDOM_MASK {
            return Err(BeeError::invalid(
                Subcode::InvalidId,
                "ULID part out of range",
            ));
        }
        Ok(Ulid((u128::from(timestamp_ms) << 80) | random))
    }

    /// Wraps a raw 128-bit value.
    pub fn from_u128(value: u128) -> Ulid {
        Ulid(value)
    }

    /// The raw 128-bit value.
    pub fn as_u128(&self) -> u128 {
        self.0
    }

    /// The 16 big-endian bytes.
    pub fn to_bytes(&self) -> [u8; 16] {
        self.0.to_be_bytes()
    }

    /// Builds a ULID from 16 big-endian bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Ulid {
        Ulid(u128::from_be_bytes(bytes))
    }

    /// The millisecond timestamp.
    pub fn timestamp_ms(&self) -> u64 {
        (self.0 >> 80) as u64
    }

    /// The 80 random bits.
    pub fn random(&self) -> u128 {
        self.0 & RANDOM_MASK
    }

    /// The canonical text form: 26 uppercase Crockford base32 characters.
    pub fn to_text(&self) -> String {
        let mut out = [0u8; ULID_TEXT_LEN];
        for (i, slot) in out.iter_mut().enumerate() {
            let shift = 5 * (ULID_TEXT_LEN - 1 - i);
            *slot = ALPHABET[((self.0 >> shift) & 0x1f) as usize];
        }
        out.iter().map(|&b| b as char).collect()
    }

    /// Parses the text form. Letters may be either case; `I`, `L`, `O` and `U` are rejected
    /// (no aliasing), and the first character must be `0`–`7` so the value fits 128 bits.
    pub fn parse(text: &str) -> Result<Ulid> {
        let bad = || BeeError::invalid(Subcode::InvalidId, format!("not a ULID: {text:?}"));
        if text.len() != ULID_TEXT_LEN {
            return Err(bad());
        }
        let mut value: u128 = 0;
        for (i, c) in text.bytes().enumerate() {
            let upper = c.to_ascii_uppercase();
            let digit = ALPHABET.iter().position(|&a| a == upper).ok_or_else(bad)? as u128;
            if i == 0 && digit > 7 {
                return Err(bad());
            }
            value = (value << 5) | digit;
        }
        Ok(Ulid(value))
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl fmt::Debug for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ulid({self})")
    }
}

impl FromStr for Ulid {
    type Err = BeeError;
    fn from_str(s: &str) -> Result<Self> {
        Ulid::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ulid_known_values() {
        assert_eq!(Ulid::from_u128(0).to_text(), "00000000000000000000000000");
        assert_eq!(
            Ulid::from_u128(u128::MAX).to_text(),
            "7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
        );
        let u = Ulid::from_parts(1, 0).unwrap();
        assert_eq!(u.to_text(), "00000000010000000000000000");
        assert_eq!(u.timestamp_ms(), 1);
        assert_eq!(u.random(), 0);
        assert_eq!(format!("{u:?}"), "Ulid(00000000010000000000000000)");
    }

    #[test]
    fn ulid_parse_rules() {
        let u = Ulid::from_parts(1_700_000_000_000, 12345).unwrap();
        assert_eq!(Ulid::parse(&u.to_text()).unwrap(), u);
        assert_eq!(Ulid::parse(&u.to_text().to_lowercase()).unwrap(), u);
        assert_eq!(u.to_string().parse::<Ulid>().unwrap(), u);
        assert!(
            Ulid::parse("8ZZZZZZZZZZZZZZZZZZZZZZZZZ").is_err(),
            "overflow"
        );
        assert!(Ulid::parse("0000000000000000000000000").is_err(), "short");
        assert!(
            Ulid::parse("0000000000000000000000000I").is_err(),
            "alias letter"
        );
        assert!(Ulid::parse("0000000000000000000000000U").is_err());
        assert!(Ulid::parse("000000000000000000000000-0").is_err());
    }

    #[test]
    fn ulid_parts_and_bytes() {
        assert!(Ulid::from_parts(MAX_TIMESTAMP_MS + 1, 0).is_err());
        assert!(Ulid::from_parts(0, 1 << 80).is_err());
        let u = Ulid::from_parts(MAX_TIMESTAMP_MS, RANDOM_MASK).unwrap();
        assert_eq!(u.as_u128(), u128::MAX);
        assert_eq!(Ulid::from_bytes(u.to_bytes()), u);
        let earlier = Ulid::from_parts(5, RANDOM_MASK).unwrap();
        let later = Ulid::from_parts(6, 0).unwrap();
        assert!(earlier < later, "time-ordered");
        assert!(
            earlier.to_text() < later.to_text(),
            "text sorts like the value"
        );
    }

    proptest! {
        #[test]
        fn ulid_round_trip(v in any::<u128>()) {
            let u = Ulid::from_u128(v);
            prop_assert_eq!(Ulid::parse(&u.to_text()).unwrap(), u);
        }
    }
}
