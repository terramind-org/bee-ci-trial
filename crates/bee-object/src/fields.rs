//! Typed access to the uint-keyed maps every object is made of, with the SPEC §4 key rules:
//! key 0 is `format_version`, keys 1–63 are defined per kind, keys ≥ 64 are extensions
//! (odd = critical, even = ignorable but preserved). Nested maps (entries, ops, provenance)
//! have no `format_version` and no extensions: any key they do not define is rejected.

use std::collections::BTreeMap;

use bee_core::{BeeError, ChangeId, Limits, ObjectId, Result, Subcode};

use crate::cbor::Value;

/// The only `format_version` defined: 1.
pub const FORMAT_VERSION: u64 = 1;
/// The first extension key.
pub const FIRST_EXTENSION_KEY: u64 = 64;

/// Extension fields of a top-level object: even keys ≥ 64 this implementation does not know,
/// kept so re-encoding reproduces the exact bytes and id.
pub type Extensions = BTreeMap<u64, Value>;

pub(crate) fn field_error(what: &str, message: impl std::fmt::Display) -> BeeError {
    BeeError::invalid(Subcode::InvalidField, format!("{what}: {message}"))
}

/// Checks that every key of a caller-supplied extension map is an even key ≥ 64 and not one
/// this implementation defines (`known`).
pub fn validate_extensions(ext: &Extensions, known: &[u64]) -> Result<()> {
    for &k in ext.keys() {
        if k < FIRST_EXTENSION_KEY || k % 2 == 1 || known.contains(&k) {
            return Err(field_error(
                "extensions",
                format!("key {k} is not an ignorable extension key"),
            ));
        }
    }
    Ok(())
}

/// Reads fields out of a decoded map, then checks nothing unexpected is left.
pub(crate) struct Fields {
    what: &'static str,
    map: BTreeMap<u64, Value>,
}

impl Fields {
    pub(crate) fn new(what: &'static str, value: Value) -> Result<Fields> {
        match value {
            Value::Map(map) => Ok(Fields { what, map }),
            _ => Err(field_error(what, "expected a map")),
        }
    }

    /// Opens a top-level object: checks the size limit, decodes, checks `format_version`.
    pub(crate) fn top(what: &'static str, bytes: &[u8], limits: &Limits) -> Result<Fields> {
        Limits::check(
            "encoded object size",
            limits.encoded_object_bytes,
            bytes.len() as u64,
        )?;
        let mut f = Fields::new(what, Value::decode(bytes)?)?;
        match f.map.remove(&0) {
            Some(Value::Uint(FORMAT_VERSION)) => Ok(f),
            Some(Value::Uint(v)) => Err(BeeError::invalid(
                Subcode::UnknownFormatVersion,
                format!("{what}: unsupported format_version {v}"),
            )),
            Some(_) => Err(field_error(
                what,
                "format_version is not an unsigned integer",
            )),
            None => Err(field_error(what, "missing format_version")),
        }
    }

    fn missing(&self, key: u64) -> BeeError {
        field_error(self.what, format!("missing field {key}"))
    }

    fn mistyped(&self, key: u64, expected: &str) -> BeeError {
        field_error(self.what, format!("field {key} must be {expected}"))
    }

    pub(crate) fn take(&mut self, key: u64) -> Option<Value> {
        self.map.remove(&key)
    }

    pub(crate) fn req(&mut self, key: u64) -> Result<Value> {
        self.take(key).ok_or_else(|| self.missing(key))
    }

    pub(crate) fn uint(&mut self, key: u64) -> Result<u64> {
        match self.req(key)? {
            Value::Uint(n) => Ok(n),
            _ => Err(self.mistyped(key, "an unsigned integer")),
        }
    }

    pub(crate) fn opt_uint(&mut self, key: u64) -> Result<Option<u64>> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Uint(n)) => Ok(Some(n)),
            Some(_) => Err(self.mistyped(key, "an unsigned integer")),
        }
    }

    pub(crate) fn bytes(&mut self, key: u64) -> Result<Vec<u8>> {
        let v = self.req(key)?;
        self.as_bytes(key, v)
    }

    pub(crate) fn opt_bytes(&mut self, key: u64) -> Result<Option<Vec<u8>>> {
        match self.take(key) {
            None => Ok(None),
            Some(v) => self.as_bytes(key, v).map(Some),
        }
    }

    fn as_bytes(&self, key: u64, v: Value) -> Result<Vec<u8>> {
        match v {
            Value::Bytes(b) => Ok(b),
            _ => Err(self.mistyped(key, "a byte string")),
        }
    }

    pub(crate) fn text(&mut self, key: u64) -> Result<String> {
        let v = self.req(key)?;
        self.as_text(key, v)
    }

    pub(crate) fn opt_text(&mut self, key: u64) -> Result<Option<String>> {
        match self.take(key) {
            None => Ok(None),
            Some(v) => self.as_text(key, v).map(Some),
        }
    }

    fn as_text(&self, key: u64, v: Value) -> Result<String> {
        match v {
            Value::Text(t) => Ok(t),
            _ => Err(self.mistyped(key, "a text string")),
        }
    }

    pub(crate) fn array(&mut self, key: u64) -> Result<Vec<Value>> {
        match self.req(key)? {
            Value::Array(a) => Ok(a),
            _ => Err(self.mistyped(key, "an array")),
        }
    }

    /// An optional array that is omitted when empty; a present empty array is rejected so
    /// each logical value has one encoding.
    pub(crate) fn opt_nonempty_array(&mut self, key: u64) -> Result<Vec<Value>> {
        match self.take(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) if !a.is_empty() => Ok(a),
            Some(Value::Array(_)) => Err(field_error(
                self.what,
                format!("field {key} is an empty array; omit it instead"),
            )),
            Some(_) => Err(self.mistyped(key, "an array")),
        }
    }

    pub(crate) fn object_id(&mut self, key: u64) -> Result<ObjectId> {
        let b = self.bytes(key)?;
        ObjectId::from_bytes(&b)
    }

    pub(crate) fn change_id(&mut self, key: u64) -> Result<ChangeId> {
        let b = self.bytes(key)?;
        ChangeId::from_bytes(&b)
    }

    /// Ends a nested map: any remaining key is an error.
    pub(crate) fn finish(self) -> Result<()> {
        match self.map.keys().next() {
            None => Ok(()),
            Some(k) => Err(field_error(self.what, format!("unknown field {k}"))),
        }
    }

    /// Ends a top-level map: unknown keys 1–63 are errors, unknown odd keys ≥ 64 are
    /// critical, unknown even keys ≥ 64 are returned to be preserved.
    pub(crate) fn finish_top(self) -> Result<Extensions> {
        let mut ext = Extensions::new();
        for (k, v) in self.map {
            if k < FIRST_EXTENSION_KEY {
                return Err(field_error(self.what, format!("unknown field {k}")));
            }
            if k % 2 == 1 {
                return Err(BeeError::invalid(
                    Subcode::UnknownCriticalExtension,
                    format!("{}: unknown critical extension {k}", self.what),
                ));
            }
            ext.insert(k, v);
        }
        Ok(ext)
    }
}

/// Starts encoding a top-level object map with `format_version` and its extensions.
pub(crate) fn top_map(ext: &Extensions) -> BTreeMap<u64, Value> {
    let mut m = ext.clone();
    m.insert(0, Value::Uint(FORMAT_VERSION));
    m
}

pub(crate) fn id_value(id: &ObjectId) -> Value {
    Value::Bytes(id.as_bytes().to_vec())
}
