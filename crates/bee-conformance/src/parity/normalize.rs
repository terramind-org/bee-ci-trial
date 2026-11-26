//! Outcomes of one call and their normalisation, so the three forms can be compared.
//!
//! Every form runs on its own copy of the same seeded server, so values fixed by the seed are
//! identical, and only values made during the call differ: ids minted by the call (ULIDs),
//! timestamps, tokens, content ids of changesets that embed a time, and the server's address.
//! [`Normalizer::normalize`] replaces
//!
//! - configured literals (each server's URL, `host:port` and token) by a placeholder;
//! - RFC 3339 timestamps, also inside longer strings, by `<time>`, and digit runs that read
//!   as a Unix time in seconds or milliseconds by `<epoch-s>` / `<epoch-ms>`;
//! - every scalar under a key ending in `At`, `AtMs` or `Ms` by `<time>`;
//! - ULIDs, runs of 32 or more hex digits, `raw` id bytes, long base64 strings, and every
//!   scalar under an id-like key ([`VOLATILE_KEYS`], or a key ending in `Url`) by a
//!   positional name (`<id:1>`, `<id:2>`, ...) assigned in order of first appearance, walking
//!   object keys in sorted order.
//!
//! [`Normalizer::outcome`] blanks a fixture's `volatile` pointers before naming, so the names
//! of the remaining values do not depend on the blanked ones.
//!
//! Positional names keep the structure: the same id in two places is the same name in both
//! forms, and a response that mentions different ids where another mentions one id does not
//! compare equal. Only absolute values are hidden.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// Keys whose scalar values are always volatile.
pub const VOLATILE_KEYS: &[&str] = &[
    "requestId",
    "token",
    "accessToken",
    "refreshToken",
    "idToken",
    "jobToken",
    "runToken",
    "installationToken",
    "clientSecret",
    "secret",
    "privateKey",
    "privateKeyPem",
    "publicKey",
    "userAgent",
    "password",
    "nonce",
    "signature",
    "etag",
    "cursor",
    "nextCursor",
    "pageToken",
    "nextPageToken",
    "deviceCode",
    "userCode",
    "code",
    "url",
    "downloadUrl",
    "uploadUrl",
    "signedUrl",
    "logsUrl",
    "archiveUrl",
];

/// What one form reported for a call.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The response messages (one for unary RPCs, every item for streams).
    Ok(Vec<Value>),
    /// The SPEC §12.2 error.
    Err(ErrorFacts),
}

/// The parts of an error document every form must agree on.
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorFacts {
    /// Reason name, e.g. `NOT_FOUND`.
    pub reason: String,
    /// The `retryable` flag.
    pub retryable: bool,
    /// The snake_case subcode, empty when absent.
    pub subcode: String,
    /// The message.
    pub message: String,
}

impl Outcome {
    /// `ok` or the error's reason.
    pub fn label(&self) -> &str {
        match self {
            Outcome::Ok(_) => "ok",
            Outcome::Err(e) => &e.reason,
        }
    }

    /// The outcome as JSON: `{"ok": [...]}` or `{"error": {...}}`.
    pub fn to_value(&self) -> Value {
        match self {
            Outcome::Ok(items) => json!({ "ok": items }),
            Outcome::Err(e) => json!({ "error": {
                "reason": e.reason,
                "retryable": e.retryable,
                "subcode": e.subcode,
                "message": e.message,
            }}),
        }
    }

    /// Keeps at most `n` stream items.
    pub fn truncate(&mut self, n: Option<u64>) {
        if let (Outcome::Ok(items), Some(n)) = (self, n) {
            items.truncate(n as usize);
        }
    }
}

/// Normalises values of one form (one server).
#[derive(Clone, Debug, Default)]
pub struct Normalizer {
    literals: Vec<(String, String)>,
}

#[derive(Default)]
struct Names {
    seen: BTreeMap<String, String>,
    next: usize,
}

impl Names {
    fn name(&mut self, value: &str) -> String {
        if let Some(n) = self.seen.get(value) {
            return n.clone();
        }
        self.next += 1;
        let n = format!("<id:{}>", self.next);
        self.seen.insert(value.to_owned(), n.clone());
        n
    }
}

impl Normalizer {
    /// A normaliser with no literals.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces `literal` (when non-empty) by `placeholder` everywhere, longest first.
    pub fn literal(mut self, literal: &str, placeholder: &str) -> Self {
        if !literal.is_empty() {
            self.literals
                .push((literal.to_owned(), placeholder.to_owned()));
            self.literals.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
        }
        self
    }

    /// The normalised form of an outcome, as JSON, with `volatile` pointers blanked.
    pub fn outcome(&self, outcome: &Outcome, volatile: &[String]) -> Value {
        // Blank first, so positional names do not depend on the blanked values.
        let mut value = outcome.to_value();
        for pointer in volatile {
            blank(&mut value, pointer);
        }
        self.normalize(&value)
    }

    /// Normalises a value (see the module docs).
    pub fn normalize(&self, value: &Value) -> Value {
        let mut names = Names::default();
        self.walk(value, Volatility::None, &mut names)
    }

    fn walk(&self, value: &Value, volatile: Volatility, names: &mut Names) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for key in keys {
                    let v = &map[key];
                    let vol = key_volatility(key);
                    let normalized = if key == "raw" && v.is_string() {
                        Value::String(names.name(v.as_str().unwrap_or_default()))
                    } else {
                        self.walk(v, vol, names)
                    };
                    out.insert(key.clone(), normalized);
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|v| self.walk(v, volatile, names))
                    .collect(),
            ),
            Value::String(s) => {
                let s = self.replace_literals(s);
                if timestamp_len(s.as_bytes()) == Some(s.len()) {
                    return Value::String("<time>".into());
                }
                match volatile {
                    Volatility::Time if !s.is_empty() => return Value::String("<time>".into()),
                    Volatility::Id if !s.is_empty() && !s.starts_with('<') => {
                        return Value::String(names.name(&s));
                    }
                    _ => {}
                }
                if is_long_base64(&s) {
                    return Value::String(names.name(&s));
                }
                Value::String(scrub(&expand_grants(&s), names))
            }
            Value::Number(_) if volatile == Volatility::Time => Value::String("<time>".into()),
            Value::Number(n) if volatile == Volatility::Id => {
                Value::String(names.name(&n.to_string()))
            }
            other => other.clone(),
        }
    }

    fn replace_literals(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (literal, placeholder) in &self.literals {
            if out.contains(literal.as_str()) {
                out = out.replace(literal.as_str(), placeholder);
            }
        }
        out
    }
}

/// How the scalar values under a key are normalised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volatility {
    /// Kept (strings are still scrubbed of embedded times and ids).
    None,
    /// A point in time or a duration: always `<time>`, so two values that happen to fall in
    /// the same millisecond in one form and not in another still compare equal.
    Time,
    /// An identifier, token or URL: a positional name.
    Id,
}

/// The volatility of a key: [`VOLATILE_KEYS`] and keys ending in `Url` are ids; keys ending
/// in `At`, `AtMs` or `Ms` are times.
pub fn key_volatility(key: &str) -> Volatility {
    if VOLATILE_KEYS.contains(&key) || key.ends_with("Url") {
        Volatility::Id
    } else if key.ends_with("At") || key.ends_with("AtMs") || key.ends_with("Ms") {
        Volatility::Time
    } else {
        Volatility::None
    }
}

/// Decodes the `g=` grant of a locally served signed URL (SPEC §10.5: base64url JSON naming the
/// repos and the caller's principal chain) in place, so [`scrub`] renames the ids inside it
/// like any other id. The grant is bound to the export's repos, which a fixture's setup may
/// create afresh on each server, so its bytes differ between forms exactly as those ids do.
fn expand_grants(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("g=") {
        let (head, tail) = rest.split_at(at + 2);
        out.push_str(head);
        let is_param = at > 0 && matches!(rest.as_bytes()[at - 1], b'?' | b'&');
        let run = tail
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            .count();
        match is_param.then(|| base64url_text(&tail[..run])).flatten() {
            Some(text) => {
                out.push_str("<grant:");
                out.push_str(&text);
                out.push('>');
            }
            None => out.push_str(&tail[..run]),
        }
        rest = &tail[run..];
    }
    out.push_str(rest);
    out
}

/// Unpadded base64url to JSON-object text, or none when it is not that.
fn base64url_text(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let mut bytes = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    let text = String::from_utf8(bytes).ok()?;
    text.starts_with('{').then_some(text)
}

fn is_long_base64(s: &str) -> bool {
    s.len() >= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_'))
        && s.bytes().any(|b| b.is_ascii_digit())
        && s.bytes().any(|b| b.is_ascii_uppercase())
}

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn is_crockford(b: u8) -> bool {
    CROCKFORD.contains(&b)
}

/// Replaces timestamps, ULIDs and long hex runs inside a string.
fn scrub(s: &str, names: &mut Names) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let boundary_before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if boundary_before {
            if let Some(len) = timestamp_len(&bytes[i..]) {
                out.push_str("<time>");
                i += len;
                continue;
            }
            let run = bytes[i..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric())
                .count();
            let token = &s[i..i + run];
            // ULIDs in either case (`run-01m5…` work refs use lower case), never mixed.
            let upper = token.to_ascii_uppercase();
            let ulid = run == 26
                && (token == upper || token == token.to_ascii_lowercase())
                && upper.bytes().all(is_crockford)
                && (b'0'..=b'7').contains(&bytes[i])
                && bytes[i..i + run].iter().any(|b| b.is_ascii_digit());
            let hex = run >= 32 && token.bytes().all(|b| b.is_ascii_hexdigit());
            if ulid || hex {
                out.push_str(&names.name(token));
                i += run;
                continue;
            }
            if let Some(placeholder) = epoch(token) {
                out.push_str(placeholder);
                i += run;
                continue;
            }
            if run > 0 {
                out.push_str(token);
                i += run;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `<epoch-ms>` or `<epoch-s>` for a run of digits that reads as a Unix time between 2017
/// and 2103 (13 digits in milliseconds, 10 in seconds), e.g. a signed URL's `expires=`.
fn epoch(token: &str) -> Option<&'static str> {
    if !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = token.parse().ok()?;
    match token.len() {
        13 if (1_500_000_000_000..4_200_000_000_000).contains(&n) => Some("<epoch-ms>"),
        10 if (1_500_000_000..4_200_000_000).contains(&n) => Some("<epoch-s>"),
        _ => None,
    }
}

/// Length of an RFC 3339 timestamp at the start of `b`, if one is there.
fn timestamp_len(b: &[u8]) -> Option<usize> {
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let at = |i: usize, c: u8| b.get(i) == Some(&c);
    let shape = (0..4).all(digit)
        && at(4, b'-')
        && digit(5)
        && digit(6)
        && at(7, b'-')
        && digit(8)
        && digit(9)
        && (at(10, b'T') || at(10, b't') || at(10, b' '))
        && digit(11)
        && digit(12)
        && at(13, b':')
        && digit(14)
        && digit(15)
        && at(16, b':')
        && digit(17)
        && digit(18);
    if !shape {
        return None;
    }
    let mut i = 19;
    if at(i, b'.') {
        i += 1;
        while digit(i) {
            i += 1;
        }
    }
    if at(i, b'Z') || at(i, b'z') {
        i += 1;
    } else if (at(i, b'+') || at(i, b'-'))
        && digit(i + 1)
        && digit(i + 2)
        && at(i + 3, b':')
        && digit(i + 4)
        && digit(i + 5)
    {
        i += 6;
    }
    Some(i)
}

/// Sets the value at `pointer` (`/a/b/0`, `*` = every element or key) to `"<volatile>"`,
/// adding the last key when its object lacks it.
pub fn blank(value: &mut Value, pointer: &str) {
    let parts: Vec<&str> = pointer.split('/').skip(1).collect();
    blank_parts(value, &parts);
}

fn blank_parts(value: &mut Value, parts: &[&str]) {
    let Some((first, rest)) = parts.split_first() else {
        *value = Value::String("<volatile>".into());
        return;
    };
    match value {
        Value::Object(map) => {
            if *first == "*" {
                for v in map.values_mut() {
                    blank_parts(v, rest);
                }
            } else if let Some(v) = map.get_mut(*first) {
                blank_parts(v, rest);
            } else if rest.is_empty() {
                // proto3-JSON omits default values: a volatile value that happens to be 0 in
                // one form is absent there, so an absent leaf is blanked too.
                map.insert((*first).to_owned(), Value::String("<volatile>".into()));
            }
        }
        Value::Array(items) => {
            if *first == "*" {
                for v in items.iter_mut() {
                    blank_parts(v, rest);
                }
            } else if let Some(v) = first.parse::<usize>().ok().and_then(|i| items.get_mut(i)) {
                blank_parts(v, rest);
            }
        }
        _ => {}
    }
}

/// The first place two values differ, as a JSON pointer with both sides, or `None`.
pub fn first_difference(a: &Value, b: &Value) -> Option<String> {
    fn go(a: &Value, b: &Value, path: &mut String) -> Option<String> {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
                keys.sort();
                keys.dedup();
                for k in keys {
                    let len = path.len();
                    path.push('/');
                    path.push_str(k);
                    let found = match (x.get(k), y.get(k)) {
                        (Some(p), Some(q)) => go(p, q, path),
                        (p, q) => Some(format!(
                            "{path}: {} vs {}",
                            short(p.unwrap_or(&Value::Null)),
                            short(q.unwrap_or(&Value::Null))
                        )),
                    };
                    if found.is_some() {
                        return found;
                    }
                    path.truncate(len);
                }
                None
            }
            (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
                for (i, (p, q)) in x.iter().zip(y).enumerate() {
                    let len = path.len();
                    path.push_str(&format!("/{i}"));
                    if let Some(f) = go(p, q, path) {
                        return Some(f);
                    }
                    path.truncate(len);
                }
                None
            }
            _ if a == b => None,
            _ => Some(format!(
                "{}: {} vs {}",
                path_or_root(path),
                short(a),
                short(b)
            )),
        }
    }
    let mut path = String::new();
    go(a, b, &mut path)
}

fn path_or_root(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

fn short(v: &Value) -> String {
    let text = v.to_string();
    if text.len() > 160 {
        let mut cut = 160;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &text[..cut])
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_names_keep_structure() {
        let n = Normalizer::new();
        let a = n.normalize(
            &json!({"id": "01JABCDEFGHJKMNPQRSTVWXYZ0", "parent": "01JABCDEFGHJKMNPQRSTVWXYZ0",
            "other": "01JZZZZZZZZZZZZZZZZZZZZZZ9"}),
        );
        let b = n.normalize(
            &json!({"id": "01JQQQQQQQQQQQQQQQQQQQQQQ1", "parent": "01JQQQQQQQQQQQQQQQQQQQQQQ1",
            "other": "01JWWWWWWWWWWWWWWWWWWWWWW2"}),
        );
        assert_eq!(a, b);
        let c = n.normalize(
            &json!({"id": "01JQQQQQQQQQQQQQQQQQQQQQQ1", "parent": "01JWWWWWWWWWWWWWWWWWWWWWW2",
            "other": "01JWWWWWWWWWWWWWWWWWWWWWW2"}),
        );
        assert_ne!(a, c);
    }

    #[test]
    fn times_literals_and_volatile_keys() {
        let n = Normalizer::new()
            .literal("http://127.0.0.1:4242", "<server>")
            .literal("127.0.0.1:4242", "<host>");
        let v = n.normalize(&json!({
            "createdAt": "2026-10-19T10:00:00.123Z",
            "note": "expires 2026-10-19T10:00:00Z, see http://127.0.0.1:4242/x",
            "updatedAtMs": 1760000000000u64,
            "title": "kept",
            "count": 3,
            "head": {"raw": "AAAA"},
            "hash": "c1_00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        }));
        assert_eq!(v["createdAt"], json!("<time>"));
        assert_eq!(v["note"], json!("expires <time>, see <server>/x"));
        assert_eq!(
            n.normalize(&json!("a.tar.gz?expires=1792382746910&n=12")),
            json!("a.tar.gz?expires=<epoch-ms>&n=12")
        );
        assert_eq!(v["title"], json!("kept"));
        assert_eq!(v["count"], json!(3));
        assert!(v["head"]["raw"].as_str().unwrap().starts_with("<id:"));
        assert!(v["hash"].as_str().unwrap().starts_with("c1_<id:"));
        assert_eq!(v["updatedAtMs"], json!("<time>"));
        // Equal and unequal times in two forms compare equal.
        let a = n.normalize(&json!({"createdAt": "2026-10-19T10:00:00Z", "updatedAt": "2026-10-19T10:00:00Z", "id": "01JABCDEFGHJKMNPQRSTVWXYZ0"}));
        let b = n.normalize(&json!({"createdAt": "2026-10-19T10:00:00Z", "updatedAt": "2026-10-19T10:00:01Z", "id": "01JQQQQQQQQQQQQQQQQQQQQQQ1"}));
        assert_eq!(a, b);
    }

    fn grant_param(json: &str) -> String {
        let t = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for ch in json.as_bytes().chunks(3) {
            let mut v = 0u32;
            for i in 0..3 {
                v = (v << 8) | u32::from(*ch.get(i).unwrap_or(&0));
            }
            for i in 0..=ch.len() {
                out.push(t[((v >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    #[test]
    fn url_grants_are_read_through() {
        let n = Normalizer::new().literal("http://s", "<server>");
        let doc = |repo: &str| {
            let g = grant_param(&format!("{{\"r\":[\"{repo}\"],\"c\":[]}}"));
            json!({"downloadUrl": format!("http://s/blob/x?exp=1&g={g}&gs=ab"), "repoIds": [repo]})
        };
        let a = n.normalize(&doc("01M5DP61GJ5QGR55DQ4AHJS6PE"));
        let b = n.normalize(&doc("01M5DP63PMB2NAJ0ZZBJWG0HN5"));
        assert_eq!(a, b);
        assert!(a["downloadUrl"].as_str().unwrap().contains("<id:"));
        // A grant for a different repo than the response names is still a difference.
        let mixed = n.normalize(&json!({
            "downloadUrl": format!("http://s/b?g={}", grant_param("{\"r\":[\"01M5DP63PMB2NAJ0ZZBJWG0HN5\"]}")),
            "repoIds": ["01M5DP61GJ5QGR55DQ4AHJS6PE"],
        }));
        assert_ne!(mixed["repoIds"], a["repoIds"]);
        let plain = n.normalize(&json!({"u": "?g=notjson"}));
        assert_eq!(plain["u"], json!("?g=notjson"));
    }

    #[test]
    fn lower_case_ulids_are_ids() {
        let n = Normalizer::new();
        let a = n.normalize(&json!("run-01m5abcdefghjkmnpqrstvwxyz"));
        let b = n.normalize(&json!("run-01m5qqqqqqqqqqqqqqqqqqqqqq"));
        assert_eq!(a, b);
        assert_eq!(a, json!("run-<id:1>"));
    }

    #[test]
    fn words_that_look_like_ids_are_kept() {
        let n = Normalizer::new();
        assert_eq!(
            n.normalize(&json!("ABCDEFGHJKMNPQRSTVWXYZABCD rest")),
            json!("ABCDEFGHJKMNPQRSTVWXYZABCD rest")
        );
        assert_eq!(n.normalize(&json!("deadbeef")), json!("deadbeef"));
    }

    #[test]
    fn blanking_and_differences() {
        let mut v = json!({"jobs": [{"status": "a"}, {"status": "b"}], "x": 1});
        blank(&mut v, "/jobs/*/status");
        blank(&mut v, "/jobs/0/durationMs");
        assert_eq!(v["jobs"][0]["durationMs"], json!("<volatile>"));
        blank(&mut v, "/missing/deep");
        assert!(v.get("missing").is_none());
        v["jobs"][0].as_object_mut().unwrap().remove("durationMs");
        assert_eq!(v["jobs"][1]["status"], json!("<volatile>"));
        assert_eq!(first_difference(&v, &v), None);
        let w = json!({"jobs": [{"status": "<volatile>"}, {"status": "<volatile>"}], "x": 2});
        assert_eq!(first_difference(&v, &w).unwrap(), "/x: 1 vs 2");
        let o = Outcome::Err(ErrorFacts {
            reason: "NOT_FOUND".into(),
            retryable: false,
            subcode: String::new(),
            message: "issue 01JABCDEFGHJKMNPQRSTVWXYZ0 not found".into(),
        });
        let p = Outcome::Err(ErrorFacts {
            message: "issue 01JQQQQQQQQQQQQQQQQQQQQQQ1 not found".into(),
            ..match &o {
                Outcome::Err(e) => e.clone(),
                Outcome::Ok(_) => unreachable!(),
            }
        });
        let n = Normalizer::new();
        assert_eq!(n.outcome(&o, &[]), n.outcome(&p, &[]));
        assert_eq!(o.label(), "NOT_FOUND");
    }
}
