//! Small helpers for encoding derived rows as sCBOR maps (the same deterministic profile as
//! objects, SPEC §4), so a derived value has exactly one byte encoding.

use std::collections::BTreeMap;

use bee_core::{BeeError, ObjectId, Reason, Result};
use bee_object::Value;

/// The error for a stored derived value that does not decode: an internal invariant broke.
pub fn corrupt(what: &str, why: impl std::fmt::Display) -> BeeError {
    BeeError::new(Reason::Internal, format!("corrupt {what} row: {why}"))
}

/// Builds a map value from `(key, value)` pairs.
pub fn map(entries: impl IntoIterator<Item = (u64, Value)>) -> Value {
    Value::Map(entries.into_iter().collect())
}

/// An id as a byte string.
pub fn id(id: &ObjectId) -> Value {
    Value::Bytes(id.as_bytes().to_vec())
}

/// A list of ids as an array of byte strings.
pub fn ids(ids: &[ObjectId]) -> Value {
    Value::Array(ids.iter().map(id).collect())
}

/// A decoded map with typed accessors that report corruption with the row's name.
pub struct Fields<'a> {
    what: &'a str,
    map: BTreeMap<u64, Value>,
}

impl<'a> Fields<'a> {
    /// Decodes `bytes` as a map.
    pub fn decode(what: &'a str, bytes: &[u8]) -> Result<Fields<'a>> {
        match Value::decode(bytes).map_err(|e| corrupt(what, e))? {
            Value::Map(map) => Ok(Fields { what, map }),
            _ => Err(corrupt(what, "not a map")),
        }
    }

    /// Wraps an already decoded map value.
    pub fn from_value(what: &'a str, value: Value) -> Result<Fields<'a>> {
        match value {
            Value::Map(map) => Ok(Fields { what, map }),
            _ => Err(corrupt(what, "not a map")),
        }
    }

    fn get(&self, key: u64) -> Result<&Value> {
        self.map
            .get(&key)
            .ok_or_else(|| corrupt(self.what, format!("missing field {key}")))
    }

    /// True when the field is present.
    pub fn has(&self, key: u64) -> bool {
        self.map.contains_key(&key)
    }

    /// A required unsigned integer.
    pub fn uint(&self, key: u64) -> Result<u64> {
        match self.get(key)? {
            Value::Uint(n) => Ok(*n),
            _ => Err(corrupt(self.what, format!("field {key} is not a uint"))),
        }
    }

    /// An optional unsigned integer.
    pub fn opt_uint(&self, key: u64) -> Result<Option<u64>> {
        if self.has(key) {
            self.uint(key).map(Some)
        } else {
            Ok(None)
        }
    }

    /// A required byte string.
    pub fn bytes(&self, key: u64) -> Result<Vec<u8>> {
        match self.get(key)? {
            Value::Bytes(b) => Ok(b.clone()),
            _ => Err(corrupt(self.what, format!("field {key} is not bytes"))),
        }
    }

    /// A required text string.
    pub fn text(&self, key: u64) -> Result<String> {
        match self.get(key)? {
            Value::Text(t) => Ok(t.clone()),
            _ => Err(corrupt(self.what, format!("field {key} is not text"))),
        }
    }

    /// A required id.
    pub fn id(&self, key: u64) -> Result<ObjectId> {
        let b = self.bytes(key)?;
        ObjectId::from_bytes(&b).map_err(|e| corrupt(self.what, e))
    }

    /// A required nested map.
    pub fn sub(&self, key: u64) -> Result<Fields<'a>> {
        Fields::from_value(self.what, self.get(key)?.clone())
    }

    /// A required array.
    pub fn array(&self, key: u64) -> Result<Vec<Value>> {
        match self.get(key)? {
            Value::Array(a) => Ok(a.clone()),
            _ => Err(corrupt(self.what, format!("field {key} is not an array"))),
        }
    }

    /// An optional array; absent reads as empty.
    pub fn opt_array(&self, key: u64) -> Result<Vec<Value>> {
        if self.has(key) {
            self.array(key)
        } else {
            Ok(Vec::new())
        }
    }

    /// An array of ids; absent reads as empty.
    pub fn ids(&self, key: u64) -> Result<Vec<ObjectId>> {
        self.opt_array(key)?
            .iter()
            .map(|v| as_id(self.what, v))
            .collect()
    }

    /// The name used in corruption errors.
    pub fn what(&self) -> &'a str {
        self.what
    }
}

/// Reads an id out of a byte-string value.
pub fn as_id(what: &str, v: &Value) -> Result<ObjectId> {
    match v {
        Value::Bytes(b) => ObjectId::from_bytes(b).map_err(|e| corrupt(what, e)),
        _ => Err(corrupt(what, "id is not bytes")),
    }
}

/// Reads a uint out of a value.
pub fn as_uint(what: &str, v: &Value) -> Result<u64> {
    match v {
        Value::Uint(n) => Ok(*n),
        _ => Err(corrupt(what, "expected a uint")),
    }
}

/// Reads bytes out of a value.
pub fn as_bytes(what: &str, v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Bytes(b) => Ok(b.clone()),
        _ => Err(corrupt(what, "expected bytes")),
    }
}

/// Reads an array out of a value.
pub fn as_array(what: &str, v: &Value) -> Result<Vec<Value>> {
    match v {
        Value::Array(a) => Ok(a.clone()),
        _ => Err(corrupt(what, "expected an array")),
    }
}

/// Lowercase hex of an encoded value, for opaque page tokens.
pub fn token(prefix: &str, v: &Value) -> String {
    format!("{prefix}{}", bee_core::hex::encode(&v.encode()))
}

/// `INVALID_ARGUMENT`/`INVALID_FIELD` for a page token this server did not write.
pub fn bad_token(text: &str) -> BeeError {
    BeeError::invalid(
        bee_core::Subcode::InvalidField,
        format!("malformed page token {text:?}"),
    )
    .with_payload("field", "page_token")
}

/// Decodes a page token written by [`token`]; `INVALID_ARGUMENT`/`INVALID_FIELD` otherwise.
pub fn parse_token(prefix: &str, text: &str) -> Result<Value> {
    let bad = || bad_token(text);
    let hex = text.strip_prefix(prefix).ok_or_else(bad)?;
    let bytes = bee_core::hex::decode(hex).ok_or_else(bad)?;
    Value::decode(&bytes).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_and_reject_junk() {
        let v = map([(1, Value::Uint(7)), (2, Value::Bytes(vec![1, 2]))]);
        let t = token("x1_", &v);
        assert_eq!(parse_token("x1_", &t).unwrap(), v);
        assert!(parse_token("x1_", "x1_zz").is_err());
        assert!(parse_token("y1_", &t).is_err());
        let f = Fields::from_value("t", v).unwrap();
        assert_eq!(f.uint(1).unwrap(), 7);
        assert_eq!(f.opt_uint(9).unwrap(), None);
        assert!(f.text(1).is_err());
        assert!(f.uint(3).is_err());
        assert!(f.ids(9).unwrap().is_empty());
        assert_eq!(f.what(), "t");
        assert!(Fields::decode("t", &[0xff]).is_err());
        assert!(Fields::decode("t", &Value::Uint(1).encode()).is_err());
    }
}
