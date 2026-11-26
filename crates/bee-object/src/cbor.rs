//! sCBOR v1 (SPEC §4): the strict deterministic CBOR profile every structured object uses.
//!
//! Allowed: unsigned and negative integers (major types 0 and 1), byte strings (2), UTF-8
//! text strings (3), arrays (4) and maps with unsigned-integer keys (5). Everything else is
//! rejected: tags, floats, simple values, indefinite lengths, non-shortest heads, unsorted or
//! duplicate map keys, non-integer map keys, invalid UTF-8 and trailing bytes.
//!
//! Map keys are compared numerically; for shortest-form unsigned keys that is the same order
//! as comparing their encoded bytes (RFC 8949 §4.2.1).

use std::collections::BTreeMap;

use bee_core::{BeeError, Result, Subcode};

/// Max nesting depth of arrays and maps. Defined objects nest at most 4 deep.
pub const MAX_DEPTH: usize = 32;

/// A decoded sCBOR value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    /// Major type 0: an unsigned integer.
    Uint(u64),
    /// Major type 1: the negative integer `-1 - n`.
    Nint(u64),
    /// Major type 2: a byte string.
    Bytes(Vec<u8>),
    /// Major type 3: a UTF-8 text string.
    Text(String),
    /// Major type 4: an array.
    Array(Vec<Value>),
    /// Major type 5: a map with unsigned-integer keys, kept sorted.
    Map(BTreeMap<u64, Value>),
}

impl Value {
    /// A signed integer, as major type 0 or 1.
    pub fn int(v: i64) -> Value {
        if v >= 0 {
            Value::Uint(v as u64)
        } else {
            // -1 - n = v  =>  n = -1 - v, which is non-negative and fits u64.
            Value::Nint((-1 - v) as u64)
        }
    }

    /// The value as a signed integer, if it is an integer in `i64` range.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Uint(n) => i64::try_from(n).ok(),
            Value::Nint(n) => i64::try_from(n).ok().map(|n| -1 - n),
            _ => None,
        }
    }

    /// Encodes the value canonically.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Appends the canonical encoding to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Uint(n) => head(out, 0, *n),
            Value::Nint(n) => head(out, 1, *n),
            Value::Bytes(b) => {
                head(out, 2, b.len() as u64);
                out.extend_from_slice(b);
            }
            Value::Text(t) => {
                head(out, 3, t.len() as u64);
                out.extend_from_slice(t.as_bytes());
            }
            Value::Array(items) => {
                head(out, 4, items.len() as u64);
                for item in items {
                    item.encode_into(out);
                }
            }
            Value::Map(map) => {
                head(out, 5, map.len() as u64);
                for (k, v) in map {
                    head(out, 0, *k);
                    v.encode_into(out);
                }
            }
        }
    }

    /// Decodes exactly one canonical value spanning all of `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<Value> {
        let mut d = Decoder {
            input: bytes,
            pos: 0,
        };
        let v = d.value(0)?;
        if d.pos != bytes.len() {
            return Err(non_canonical(format!(
                "{} trailing bytes after the value",
                bytes.len() - d.pos
            )));
        }
        Ok(v)
    }
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= u8::MAX as u64 {
        out.push(m | 24);
        out.push(n as u8);
    } else if n <= u16::MAX as u64 {
        out.push(m | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= u32::MAX as u64 {
        out.push(m | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

/// An `INVALID_ARGUMENT`/`NON_CANONICAL` error.
pub fn non_canonical(message: impl Into<String>) -> BeeError {
    BeeError::invalid(Subcode::NonCanonical, message)
}

/// Reads the item heads at the start of an encoding one by one, without decoding what
/// follows. For callers that need one leading field of a large object (the root id of a
/// changeset with a large delta) and may hold only a prefix of its bytes.
pub(crate) struct HeadReader<'a> {
    d: Decoder<'a>,
}

impl<'a> HeadReader<'a> {
    pub(crate) fn new(input: &'a [u8]) -> HeadReader<'a> {
        HeadReader {
            d: Decoder { input, pos: 0 },
        }
    }

    /// The next head as (major type, argument), checked for shortest form.
    pub(crate) fn head(&mut self) -> Result<(u8, u64)> {
        self.d.head()
    }

    /// The next `n` bytes (a byte string's content after its head).
    pub(crate) fn take(&mut self, n: u64) -> Result<&[u8]> {
        let len = self.d.length(n)?;
        self.d.take(len)
    }
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
}

impl Decoder<'_> {
    fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if n > self.remaining() {
            return Err(non_canonical(format!("truncated at byte {}", self.pos)));
        }
        let s = &self.input[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Reads a head, returning (major type, argument).
    fn head(&mut self) -> Result<(u8, u64)> {
        let at = self.pos;
        let first = self.take(1)?[0];
        let major = first >> 5;
        let info = first & 0x1f;
        let (n, min) = match info {
            0..=23 => return Ok((major, u64::from(info))),
            24 => (u64::from(self.take(1)?[0]), 24),
            25 => {
                let b = self.take(2)?;
                (u64::from(u16::from_be_bytes([b[0], b[1]])), 1 << 8)
            }
            26 => {
                let b = self.take(4)?;
                (
                    u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])),
                    1 << 16,
                )
            }
            27 => {
                let b = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                (u64::from_be_bytes(a), 1 << 32)
            }
            31 => return Err(non_canonical(format!("indefinite length at byte {at}"))),
            _ => {
                return Err(non_canonical(format!(
                    "reserved additional info at byte {at}"
                )));
            }
        };
        if n < min {
            return Err(non_canonical(format!(
                "non-shortest integer or length at byte {at}"
            )));
        }
        Ok((major, n))
    }

    fn length(&mut self, n: u64) -> Result<usize> {
        let n = usize::try_from(n).map_err(|_| non_canonical("length overflows"))?;
        if n > self.remaining() {
            return Err(non_canonical(format!("length {n} exceeds remaining input")));
        }
        Ok(n)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        let at = self.pos;
        let (major, n) = self.head()?;
        match major {
            0 => Ok(Value::Uint(n)),
            1 => Ok(Value::Nint(n)),
            2 => {
                let len = self.length(n)?;
                Ok(Value::Bytes(self.take(len)?.to_vec()))
            }
            3 => {
                let len = self.length(n)?;
                let raw = self.take(len)?.to_vec();
                String::from_utf8(raw)
                    .map(Value::Text)
                    .map_err(|_| non_canonical(format!("invalid UTF-8 in text at byte {at}")))
            }
            4 | 5 => {
                if depth >= MAX_DEPTH {
                    return Err(non_canonical("nesting too deep"));
                }
                // Each item takes at least one byte, so a count beyond the input is bogus.
                let count = self.length(n)?;
                if major == 4 {
                    let mut items = Vec::with_capacity(count);
                    for _ in 0..count {
                        items.push(self.value(depth + 1)?);
                    }
                    Ok(Value::Array(items))
                } else {
                    let mut map = BTreeMap::new();
                    let mut last: Option<u64> = None;
                    for _ in 0..count {
                        let key_at = self.pos;
                        let (kmajor, key) = self.head()?;
                        if kmajor != 0 {
                            return Err(non_canonical(format!(
                                "map key at byte {key_at} is not an unsigned integer"
                            )));
                        }
                        if last.is_some_and(|l| key <= l) {
                            return Err(non_canonical(format!(
                                "map key {key} at byte {key_at} is unsorted or duplicated"
                            )));
                        }
                        last = Some(key);
                        let v = self.value(depth + 1)?;
                        map.insert(key, v);
                    }
                    Ok(Value::Map(map))
                }
            }
            6 => Err(non_canonical(format!("tag at byte {at}"))),
            _ => Err(non_canonical(format!("float or simple value at byte {at}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_core::hex;
    use proptest::prelude::*;

    fn enc(v: &Value) -> String {
        hex::encode(&v.encode())
    }

    fn rejects(hex_text: &str) {
        let bytes = hex::decode(hex_text).unwrap();
        let err = Value::decode(&bytes).unwrap_err();
        assert!(err.is_invalid(Subcode::NonCanonical), "{hex_text}: {err}");
    }

    #[test]
    fn integers_use_shortest_form() {
        assert_eq!(enc(&Value::Uint(0)), "00");
        assert_eq!(enc(&Value::Uint(23)), "17");
        assert_eq!(enc(&Value::Uint(24)), "1818");
        assert_eq!(enc(&Value::Uint(255)), "18ff");
        assert_eq!(enc(&Value::Uint(256)), "190100");
        assert_eq!(enc(&Value::Uint(65535)), "19ffff");
        assert_eq!(enc(&Value::Uint(65536)), "1a00010000");
        assert_eq!(enc(&Value::Uint(u32::MAX as u64)), "1affffffff");
        assert_eq!(enc(&Value::Uint(1 << 32)), "1b0000000100000000");
        assert_eq!(enc(&Value::Uint(u64::MAX)), "1bffffffffffffffff");
        assert_eq!(enc(&Value::int(-1)), "20");
        assert_eq!(enc(&Value::int(-24)), "37");
        assert_eq!(enc(&Value::int(-25)), "3818");
        assert_eq!(enc(&Value::int(i64::MIN)), "3b7fffffffffffffff");
    }

    #[test]
    fn strings_arrays_maps() {
        assert_eq!(enc(&Value::Bytes(vec![])), "40");
        assert_eq!(enc(&Value::Bytes(vec![1, 2])), "420102");
        assert_eq!(enc(&Value::Text("a".into())), "6161");
        assert_eq!(enc(&Value::Array(vec![Value::Uint(1)])), "8101");
        let mut m = BTreeMap::new();
        m.insert(24, Value::Uint(0));
        m.insert(1, Value::Uint(0));
        assert_eq!(enc(&Value::Map(m)), "a20100181800");
    }

    #[test]
    fn int_conversions() {
        for v in [0i64, 1, -1, 23, -24, -25, i64::MAX, i64::MIN] {
            assert_eq!(Value::int(v).as_i64(), Some(v));
            assert_eq!(
                Value::decode(&Value::int(v).encode()).unwrap(),
                Value::int(v)
            );
        }
        assert_eq!(Value::Uint(u64::MAX).as_i64(), None);
        assert_eq!(Value::Nint(u64::MAX).as_i64(), None);
        assert_eq!(Value::Text("x".into()).as_i64(), None);
    }

    #[test]
    fn rejects_non_shortest_heads() {
        rejects("1817"); // 23 in one extra byte
        rejects("190017"); // 23 in two bytes
        rejects("1900ff"); // 255 in two bytes
        rejects("1a0000ffff");
        rejects("1b00000000ffffffff");
        rejects("3817"); // negative, non-shortest
        rejects("5801ff"); // byte-string length 1 in long form
        rejects("780161"); // text length 1 in long form
        rejects("980101"); // array count in long form
        rejects("b8010000"); // map count in long form
    }

    #[test]
    fn rejects_indefinite_tags_floats_simple() {
        rejects("5f41ff"); // indefinite byte string
        rejects("7f6161ff"); // indefinite text
        rejects("9f01ff"); // indefinite array
        rejects("bf0101ff"); // indefinite map
        rejects("c100"); // tag 1
        rejects("d81800"); // tag 24
        rejects("f4"); // false
        rejects("f6"); // null
        rejects("f97e00"); // half float NaN
        rejects("fa3f800000"); // float32
        rejects("fb3ff0000000000000"); // float64
        rejects("1c"); // reserved additional info 28
        rejects("1d");
        rejects("1e");
        rejects("ff"); // break outside indefinite item
    }

    #[test]
    fn rejects_bad_maps() {
        rejects("a202010101"); // keys 2 then 1
        rejects("a201010101"); // duplicate key 1
        rejects("a12001"); // negative key
        rejects("a1616101"); // text key
        rejects("a1410101"); // byte-string key
        rejects("a1810101"); // array key
    }

    #[test]
    fn rejects_truncation_trailing_and_bad_utf8() {
        rejects("");
        rejects("18");
        rejects("4201");
        rejects("8201");
        rejects("a101");
        rejects("0000"); // trailing byte
        rejects("62c328"); // invalid UTF-8
        rejects("5bffffffffffffffff"); // absurd length
        rejects("9bffffffffffffffff"); // absurd count
    }

    #[test]
    fn rejects_excess_nesting() {
        let mut bytes = vec![0x81; MAX_DEPTH + 1];
        bytes.push(0x00);
        rejects(&hex::encode(&bytes));
        let mut ok = vec![0x81; MAX_DEPTH];
        ok.push(0x00);
        assert!(Value::decode(&ok).is_ok());
    }

    fn arb_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            any::<u64>().prop_map(Value::Uint),
            any::<u64>().prop_map(Value::Nint),
            proptest::collection::vec(any::<u8>(), 0..40).prop_map(Value::Bytes),
            ".{0,20}".prop_map(Value::Text),
        ];
        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                proptest::collection::btree_map(any::<u64>(), inner, 0..6).prop_map(Value::Map),
            ]
        })
    }

    proptest! {
        #[test]
        fn round_trip(v in arb_value()) {
            let bytes = v.encode();
            prop_assert_eq!(Value::decode(&bytes).unwrap(), v.clone());
            // Re-encoding a decoded value reproduces the bytes exactly.
            prop_assert_eq!(Value::decode(&bytes).unwrap().encode(), bytes);
        }

        #[test]
        fn trailing_bytes_always_rejected(v in arb_value(), extra in any::<u8>()) {
            let mut bytes = v.encode();
            bytes.push(extra);
            prop_assert!(Value::decode(&bytes).is_err());
        }

        #[test]
        fn decoder_never_panics_and_accepts_only_canonical(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            if let Ok(v) = Value::decode(&bytes) {
                prop_assert_eq!(v.encode(), bytes);
            }
        }

        #[test]
        fn widened_heads_rejected(n in any::<u64>()) {
            // Encode n in every head width wider than its shortest form; all must fail.
            let shortest = Value::Uint(n).encode();
            for (info, width) in [(24u8, 1usize), (25, 2), (26, 4), (27, 8)] {
                if width < shortest.len() || (width < 8 && n >> (width * 8) != 0) {
                    continue;
                }
                let mut bytes = vec![info];
                bytes.extend_from_slice(&n.to_be_bytes()[8 - width..]);
                prop_assert!(Value::decode(&bytes).is_err(), "{:?}", bytes);
            }
        }
    }
}
