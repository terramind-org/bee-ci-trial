//! Package URLs (purl): construction, rendering and parsing.
//!
//! The subset implemented is the one the dependency graph needs: `pkg:type/namespace/name@version`
//! with optional qualifiers and subpath. Rendering is canonical and deterministic.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::{DepsError, Result};

/// A parsed or constructed package URL.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Purl {
    /// The package type, lowercase (`cargo`, `npm`, `pypi`, ...).
    pub ty: String,
    /// Namespace segments, decoded (for example `["@babel"]` or `["github.com", "spf13"]`).
    pub namespace: Vec<String>,
    /// The package name, decoded.
    pub name: String,
    /// The version, decoded.
    pub version: Option<String>,
    /// Qualifiers, sorted by key.
    pub qualifiers: BTreeMap<String, String>,
    /// Subpath segments, decoded.
    pub subpath: Vec<String>,
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~')
}

fn encode(s: &str, out: &mut String) {
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b & 15)]));
        }
    }
}

fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl Purl {
    /// A purl with no namespace, qualifiers or subpath.
    pub fn simple(ty: &str, name: &str, version: Option<&str>) -> Purl {
        Purl {
            ty: ty.to_owned(),
            namespace: Vec::new(),
            name: name.to_owned(),
            version: version.filter(|v| !v.is_empty()).map(str::to_owned),
            qualifiers: BTreeMap::new(),
            subpath: Vec::new(),
        }
    }

    /// The purl without its version, qualifiers and subpath, as a string (a package identity).
    pub fn without_version(&self) -> String {
        let mut p = self.clone();
        p.version = None;
        p.qualifiers.clear();
        p.subpath.clear();
        p.to_string()
    }

    /// Parses a package URL string.
    ///
    /// Fails with `INVALID_ARGUMENT` for a missing `pkg:` scheme, an empty type or name, an
    /// invalid type character, or a bad percent escape.
    pub fn parse(text: &str) -> Result<Purl> {
        let bad =
            |why: &str| DepsError::invalid("", format!("invalid package URL {text:?}: {why}"));
        let rest = text
            .strip_prefix("pkg:")
            .ok_or_else(|| bad("missing pkg: scheme"))?;
        let rest = rest.trim_start_matches('/');
        let (rest, subpath) = match rest.split_once('#') {
            Some((r, s)) => (r, Some(s)),
            None => (rest, None),
        };
        let (rest, qual) = match rest.split_once('?') {
            Some((r, q)) => (r, Some(q)),
            None => (rest, None),
        };
        let (rest, version) = match rest.rsplit_once('@') {
            Some((r, v)) => (r, Some(v)),
            None => (rest, None),
        };
        let (ty, path) = rest
            .split_once('/')
            .ok_or_else(|| bad("missing type or name"))?;
        if ty.is_empty()
            || !ty.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-')
            })
        {
            return Err(bad("invalid type"));
        }
        let mut segs: Vec<String> = Vec::new();
        for s in path.split('/') {
            if s.is_empty() {
                continue;
            }
            segs.push(decode(s).ok_or_else(|| bad("bad percent escape"))?);
        }
        let name = segs.pop().ok_or_else(|| bad("missing name"))?;
        let version = match version {
            Some(v) if !v.is_empty() => Some(decode(v).ok_or_else(|| bad("bad percent escape"))?),
            _ => None,
        };
        let mut qualifiers = BTreeMap::new();
        if let Some(q) = qual {
            for pair in q.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| bad("qualifier without value"))?;
                let v = decode(v).ok_or_else(|| bad("bad percent escape"))?;
                if !v.is_empty() {
                    qualifiers.insert(k.to_ascii_lowercase(), v);
                }
            }
        }
        let mut sub = Vec::new();
        if let Some(s) = subpath {
            for seg in s
                .split('/')
                .filter(|p| !p.is_empty() && *p != "." && *p != "..")
            {
                sub.push(decode(seg).ok_or_else(|| bad("bad percent escape"))?);
            }
        }
        Ok(Purl {
            ty: ty.to_owned(),
            namespace: segs,
            name,
            version,
            qualifiers,
            subpath: sub,
        })
    }
}

impl fmt::Display for Purl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::from("pkg:");
        s.push_str(&self.ty);
        s.push('/');
        for ns in &self.namespace {
            encode(ns, &mut s);
            s.push('/');
        }
        encode(&self.name, &mut s);
        if let Some(v) = &self.version {
            s.push('@');
            encode(v, &mut s);
        }
        if !self.qualifiers.is_empty() {
            s.push('?');
            let mut first = true;
            for (k, v) in &self.qualifiers {
                if !first {
                    s.push('&');
                }
                first = false;
                s.push_str(k);
                s.push('=');
                encode(v, &mut s);
            }
        }
        if !self.subpath.is_empty() {
            s.push('#');
            let mut first = true;
            for seg in &self.subpath {
                if !first {
                    s.push('/');
                }
                first = false;
                encode(seg, &mut s);
            }
        }
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_parses_round_trip() {
        let mut p = Purl::simple("npm", "core", Some("7.0.0+build"));
        p.namespace = vec!["@babel".into()];
        assert_eq!(p.to_string(), "pkg:npm/%40babel/core@7.0.0%2Bbuild");
        assert_eq!(Purl::parse(&p.to_string()).unwrap(), p);
    }

    #[test]
    fn qualifiers_and_subpath() {
        let p = Purl::parse("pkg:githubactions/github/codeql-action@v3#init").unwrap();
        assert_eq!(p.namespace, vec!["github"]);
        assert_eq!(p.name, "codeql-action");
        assert_eq!(p.subpath, vec!["init"]);
        assert_eq!(
            p.to_string(),
            "pkg:githubactions/github/codeql-action@v3#init"
        );
        let q = Purl::parse("pkg:maven/org.x/y@1?type=jar&classifier=src").unwrap();
        assert_eq!(q.to_string(), "pkg:maven/org.x/y@1?classifier=src&type=jar");
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "npm/x",
            "pkg:",
            "pkg:npm",
            "pkg:NPM/x",
            "pkg:npm/%zz@1",
            "pkg:npm/x?a",
        ] {
            assert!(Purl::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn without_version_drops_everything_after_name() {
        let p = Purl::parse("pkg:cargo/serde@1.0.0?a=b#c").unwrap();
        assert_eq!(p.without_version(), "pkg:cargo/serde");
    }
}
