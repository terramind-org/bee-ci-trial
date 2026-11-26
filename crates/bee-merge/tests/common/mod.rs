//! Parser for the `example` blocks of `docs/contracts/diff3-v1.md`.

#![allow(dead_code)]

use std::collections::BTreeMap;

/// The contract text.
pub fn contract() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/contracts/diff3-v1.md"
    );
    std::fs::read_to_string(path).expect("contract is readable")
}

/// One example: its fields in file order.
#[derive(Debug, Clone)]
pub struct Example {
    pub fields: BTreeMap<String, String>,
}

impl Example {
    pub fn id(&self) -> &str {
        &self.fields["id"]
    }

    pub fn raw(&self, key: &str) -> &str {
        self.fields
            .get(key)
            .unwrap_or_else(|| panic!("{}: missing {key}", self.id()))
    }

    pub fn has(&self, key: &str) -> bool {
        self.fields.contains_key(key)
    }

    /// A quoted string field, unescaped.
    pub fn text(&self, key: &str) -> Vec<u8> {
        unquote(self.raw(key)).unwrap_or_else(|e| panic!("{}: {key}: {e}", self.id()))
    }

    /// A list of comma-separated integer tuples, or `none`.
    pub fn tuples(&self, key: &str) -> Vec<Vec<usize>> {
        let raw = self.raw(key);
        if raw == "none" {
            return Vec::new();
        }
        raw.split_whitespace()
            .map(|t| t.split(',').map(|n| n.parse().expect("integer")).collect())
            .collect()
    }

    pub fn numbers(&self, key: &str) -> Vec<usize> {
        self.raw(key)
            .split_whitespace()
            .map(|n| n.parse().expect("integer"))
            .collect()
    }
}

/// Every example block, in file order.
pub fn examples() -> Vec<Example> {
    let text = contract();
    let mut out = Vec::new();
    let mut current: Option<BTreeMap<String, String>> = None;
    for line in text.lines() {
        match &mut current {
            None if line.trim_end() == "```example" => current = Some(BTreeMap::new()),
            None => {}
            Some(fields) if line.trim_end() == "```" => {
                out.push(Example {
                    fields: std::mem::take(fields),
                });
                current = None;
            }
            Some(fields) => {
                let (key, value) = line.split_once(": ").expect("key: value line");
                let prev = fields.insert(key.to_string(), value.to_string());
                assert!(prev.is_none(), "duplicate key {key}");
            }
        }
    }
    assert!(current.is_none(), "unterminated example block");
    out
}

/// Parses a double-quoted string with `\n`, `\r`, `\t`, `\0`, `\\` and `\"` escapes.
pub fn unquote(raw: &str) -> Result<Vec<u8>, String> {
    let inner = raw
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .ok_or_else(|| format!("not quoted: {raw}"))?;
    let mut out = Vec::new();
    let mut chars = inner.bytes();
    while let Some(c) = chars.next() {
        if c != b'\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b't') => out.push(b'\t'),
            Some(b'0') => out.push(0),
            Some(b'\\') => out.push(b'\\'),
            Some(b'"') => out.push(b'"'),
            other => return Err(format!("bad escape {other:?}")),
        }
    }
    Ok(out)
}
