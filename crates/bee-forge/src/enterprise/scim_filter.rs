//! SCIM 2.0 filters (RFC 7644 §3.4.2.2) and PATCH operations (§3.5.2) over JSON resources.
//!
//! Both work on the JSON form of a resource, so Users, Groups and any future resource type
//! share them. Attribute names are case-insensitive; string comparison is case-insensitive
//! except for ids and external ids.
//!
//! Filters: `pr`, `eq ne co sw ew gt ge lt le`, `and`, `or`, `not(...)`, parentheses, dotted
//! sub-attributes (`name.familyName`, `emails.value`) and value paths
//! (`emails[type eq "work" and primary eq true]`). A multi-valued attribute matches when any
//! element does.
//!
//! PATCH paths: `attr`, `attr.sub`, `attr[filter]`, `attr[filter].sub`, optionally prefixed
//! with the core schema URN. `add` appends to multi-valued attributes (no duplicates by
//! `value`), `replace` overwrites, `remove` deletes the attribute, the matching elements, or
//! the listed `value` elements. Operation names are case-insensitive (Entra sends `Replace`),
//! and the string booleans `"True"`/`"False"` that Entra sends for `active` are coerced.

use serde_json::{Map, Value, json};

/// An error in SCIM's own format (RFC 7644 §3.12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScimError {
    /// HTTP status.
    pub status: u16,
    /// `scimType`, when the RFC names one (`invalidFilter`, `uniqueness`, `noTarget`, ...).
    pub scim_type: &'static str,
    /// Human-readable detail.
    pub detail: String,
}

impl ScimError {
    /// An error with a status, type and detail.
    pub fn new(status: u16, scim_type: &'static str, detail: impl Into<String>) -> ScimError {
        ScimError {
            status,
            scim_type,
            detail: detail.into(),
        }
    }

    /// The JSON body of the error.
    pub fn to_json(&self) -> Value {
        let mut o = json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:Error"],
            "status": self.status.to_string(),
            "detail": self.detail,
        });
        if !self.scim_type.is_empty() {
            o["scimType"] = json!(self.scim_type);
        }
        o
    }
}

/// A comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Contains.
    Co,
    /// Starts with.
    Sw,
    /// Ends with.
    Ew,
    /// Greater than.
    Gt,
    /// Greater or equal.
    Ge,
    /// Less than.
    Lt,
    /// Less or equal.
    Le,
}

/// A parsed filter.
#[derive(Clone, Debug, PartialEq)]
pub enum Filter {
    /// `path op value`.
    Cmp(String, Op, Value),
    /// `path pr`.
    Present(String),
    /// `a and b`.
    And(Box<Filter>, Box<Filter>),
    /// `a or b`.
    Or(Box<Filter>, Box<Filter>),
    /// `not (a)`.
    Not(Box<Filter>),
    /// `attr[inner]`: some element of the multi-valued attribute matches `inner`.
    Has(String, Box<Filter>),
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Num(f64),
    LParen,
    RParen,
    LBracket,
    RBracket,
}

fn invalid_filter(msg: impl Into<String>) -> ScimError {
    ScimError::new(400, "invalidFilter", msg)
}

fn lex(s: &str) -> Result<Vec<Tok>, ScimError> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            ' ' | '\t' | '\n' => i += 1,
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            '"' => {
                let mut v = String::new();
                i += 1;
                loop {
                    let Some(&ch) = b.get(i) else {
                        return Err(invalid_filter("unterminated string"));
                    };
                    i += 1;
                    match ch {
                        '"' => break,
                        '\\' => {
                            let Some(&e) = b.get(i) else {
                                return Err(invalid_filter("bad escape"));
                            };
                            i += 1;
                            v.push(match e {
                                'n' => '\n',
                                't' => '\t',
                                'r' => '\r',
                                other => other,
                            });
                        }
                        ch => v.push(ch),
                    }
                }
                out.push(Tok::Str(v));
            }
            c if c.is_ascii_digit() || c == '-' => {
                let st = i;
                i += 1;
                while i < b.len()
                    && (b[i].is_ascii_digit() || b[i] == '.' || b[i] == 'e' || b[i] == 'E')
                {
                    i += 1;
                }
                let t: String = b[st..i].iter().collect();
                out.push(Tok::Num(
                    t.parse()
                        .map_err(|_| invalid_filter(format!("bad number {t}")))?,
                ));
            }
            c if c.is_alphabetic() || c == '$' || c == '_' => {
                let st = i;
                while i < b.len()
                    && (b[i].is_alphanumeric() || matches!(b[i], '.' | '_' | '-' | ':' | '$'))
                {
                    i += 1;
                }
                out.push(Tok::Word(b[st..i].iter().collect()));
            }
            other => return Err(invalid_filter(format!("unexpected character {other:?}"))),
        }
    }
    Ok(out)
}

struct Parser {
    t: Vec<Tok>,
    p: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.t.get(self.p)
    }

    fn word_is(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn or(&mut self) -> Result<Filter, ScimError> {
        let mut l = self.and()?;
        while self.word_is("or") {
            self.p += 1;
            let r = self.and()?;
            l = Filter::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and(&mut self) -> Result<Filter, ScimError> {
        let mut l = self.unary()?;
        while self.word_is("and") {
            self.p += 1;
            let r = self.unary()?;
            l = Filter::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn unary(&mut self) -> Result<Filter, ScimError> {
        if self.word_is("not") {
            self.p += 1;
            if self.peek() != Some(&Tok::LParen) {
                return Err(invalid_filter("`not` needs parentheses"));
            }
            self.p += 1;
            let f = self.or()?;
            self.expect(Tok::RParen)?;
            return Ok(Filter::Not(Box::new(f)));
        }
        if self.peek() == Some(&Tok::LParen) {
            self.p += 1;
            let f = self.or()?;
            self.expect(Tok::RParen)?;
            return Ok(f);
        }
        let Some(Tok::Word(path)) = self.peek().cloned() else {
            return Err(invalid_filter("expected an attribute"));
        };
        self.p += 1;
        if self.peek() == Some(&Tok::LBracket) {
            self.p += 1;
            let inner = self.or()?;
            self.expect(Tok::RBracket)?;
            return Ok(Filter::Has(path, Box::new(inner)));
        }
        let Some(Tok::Word(op)) = self.peek().cloned() else {
            return Err(invalid_filter("expected an operator"));
        };
        self.p += 1;
        let op = match op.to_ascii_lowercase().as_str() {
            "pr" => return Ok(Filter::Present(path)),
            "eq" => Op::Eq,
            "ne" => Op::Ne,
            "co" => Op::Co,
            "sw" => Op::Sw,
            "ew" => Op::Ew,
            "gt" => Op::Gt,
            "ge" => Op::Ge,
            "lt" => Op::Lt,
            "le" => Op::Le,
            other => return Err(invalid_filter(format!("unknown operator `{other}`"))),
        };
        let v = match self.peek().cloned() {
            Some(Tok::Str(s)) => Value::String(s),
            Some(Tok::Num(n)) => json!(n),
            Some(Tok::Word(w)) => match w.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "null" => Value::Null,
                other => return Err(invalid_filter(format!("bad value `{other}`"))),
            },
            _ => return Err(invalid_filter("expected a value")),
        };
        self.p += 1;
        Ok(Filter::Cmp(path, op, v))
    }

    fn expect(&mut self, t: Tok) -> Result<(), ScimError> {
        if self.peek() == Some(&t) {
            self.p += 1;
            Ok(())
        } else {
            Err(invalid_filter(format!("expected {t:?}")))
        }
    }
}

/// Parses a filter expression.
pub fn parse_filter(s: &str) -> Result<Filter, ScimError> {
    let mut p = Parser { t: lex(s)?, p: 0 };
    if p.t.is_empty() {
        return Err(invalid_filter("empty filter"));
    }
    let f = p.or()?;
    if p.p != p.t.len() {
        return Err(invalid_filter("unexpected trailing input"));
    }
    Ok(f)
}

/// Case-insensitive object member lookup.
pub fn get_ci<'a>(o: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    o.get(name).or_else(|| {
        o.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    })
}

fn key_ci(o: &Map<String, Value>, name: &str) -> Option<String> {
    o.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned()
}

fn strip_urn(path: &str) -> &str {
    for p in [
        "urn:ietf:params:scim:schemas:core:2.0:User:",
        "urn:ietf:params:scim:schemas:core:2.0:Group:",
    ] {
        if let Some(r) = path.strip_prefix(p) {
            return r;
        }
    }
    path
}

fn leaves<'a>(v: &'a Value, path: &[&str], out: &mut Vec<&'a Value>) {
    match (v, path.split_first()) {
        (Value::Array(a), _) => a.iter().for_each(|x| leaves(x, path, out)),
        (_, None) => out.push(v),
        (Value::Object(o), Some((head, rest))) => {
            if let Some(n) = get_ci(o, head) {
                leaves(n, rest, out);
            }
        }
        _ => {}
    }
}

fn case_exact(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    matches!(
        p.as_str(),
        "id" | "externalid" | "members.value" | "groups.value" | "$ref" | "value"
    )
}

fn cmp_value(path: &str, op: Op, have: &Value, want: &Value) -> bool {
    use std::cmp::Ordering;
    let ord = match (have, want) {
        (Value::String(a), Value::String(b)) => {
            if case_exact(path) {
                Some(a.cmp(b))
            } else {
                Some(a.to_lowercase().cmp(&b.to_lowercase()))
            }
        }
        (Value::Number(a), Value::Number(b)) => a.as_f64().partial_cmp(&b.as_f64()),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        _ => None,
    };
    match op {
        Op::Eq => ord == Some(Ordering::Equal),
        Op::Ne => ord != Some(Ordering::Equal),
        Op::Gt => ord == Some(Ordering::Greater),
        Op::Ge => matches!(ord, Some(Ordering::Greater | Ordering::Equal)),
        Op::Lt => ord == Some(Ordering::Less),
        Op::Le => matches!(ord, Some(Ordering::Less | Ordering::Equal)),
        Op::Co | Op::Sw | Op::Ew => match (have, want) {
            (Value::String(a), Value::String(b)) => {
                let (a, b) = if case_exact(path) {
                    (a.clone(), b.clone())
                } else {
                    (a.to_lowercase(), b.to_lowercase())
                };
                match op {
                    Op::Co => a.contains(&b),
                    Op::Sw => a.starts_with(&b),
                    _ => a.ends_with(&b),
                }
            }
            _ => false,
        },
    }
}

impl Filter {
    /// Whether the resource matches.
    pub fn matches(&self, r: &Value) -> bool {
        match self {
            Filter::And(a, b) => a.matches(r) && b.matches(r),
            Filter::Or(a, b) => a.matches(r) || b.matches(r),
            Filter::Not(a) => !a.matches(r),
            Filter::Present(p) => {
                let mut out = Vec::new();
                leaves(r, &strip_urn(p).split('.').collect::<Vec<_>>(), &mut out);
                out.iter().any(|v| {
                    !matches!(v, Value::Null) && !matches!(v, Value::String(s) if s.is_empty())
                })
            }
            Filter::Cmp(p, op, want) => {
                let mut out = Vec::new();
                leaves(r, &strip_urn(p).split('.').collect::<Vec<_>>(), &mut out);
                if out.is_empty() {
                    return *op == Op::Ne && !matches!(want, Value::Null);
                }
                if *op == Op::Ne {
                    return out.iter().all(|h| cmp_value(p, *op, h, want));
                }
                out.iter().any(|h| cmp_value(p, *op, h, want))
            }
            Filter::Has(p, inner) => {
                let mut out = Vec::new();
                leaves(r, &strip_urn(p).split('.').collect::<Vec<_>>(), &mut out);
                out.iter().any(|e| inner.matches(e))
            }
        }
    }
}

// ------------------------------------------------------------------ PATCH

/// One PATCH operation.
#[derive(Clone, Debug)]
pub struct PatchOp {
    /// `add`, `replace` or `remove` (any case).
    pub op: String,
    /// The target path, if any.
    pub path: Option<String>,
    /// The value, if any.
    pub value: Option<Value>,
}

/// Parses the `Operations` of a PATCH body.
pub fn parse_patch(body: &Value) -> Result<Vec<PatchOp>, ScimError> {
    let ops = body
        .get("Operations")
        .or_else(|| body.get("operations"))
        .and_then(Value::as_array)
        .ok_or_else(|| ScimError::new(400, "invalidSyntax", "PATCH needs an Operations array"))?;
    if ops.is_empty() {
        return Err(ScimError::new(400, "invalidSyntax", "Operations is empty"));
    }
    ops.iter()
        .map(|o| {
            Ok(PatchOp {
                op: o
                    .get("op")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ScimError::new(400, "invalidSyntax", "operation has no op"))?
                    .to_ascii_lowercase(),
                path: o.get("path").and_then(Value::as_str).map(str::to_string),
                value: o.get("value").cloned(),
            })
        })
        .collect()
}

struct Path {
    attr: String,
    filter: Option<Filter>,
    sub: Option<String>,
}

fn parse_path(raw: &str) -> Result<Path, ScimError> {
    let p = strip_urn(raw.trim());
    let (attr_part, rest) = match p.find('[') {
        Some(i) => {
            let close = p
                .rfind(']')
                .filter(|c| *c > i)
                .ok_or_else(|| ScimError::new(400, "invalidPath", "unterminated value path"))?;
            let f = parse_filter(&p[i + 1..close])
                .map_err(|e| ScimError::new(400, "invalidPath", e.detail))?;
            (&p[..i], (Some(f), p[close + 1..].strip_prefix('.')))
        }
        None => match p.split_once('.') {
            Some((a, s)) => (a, (None, Some(s))),
            None => (p, (None, None)),
        },
    };
    if attr_part.is_empty() {
        return Err(ScimError::new(400, "invalidPath", "empty path"));
    }
    Ok(Path {
        attr: attr_part.to_string(),
        filter: rest.0,
        sub: rest.1.filter(|s| !s.is_empty()).map(str::to_string),
    })
}

fn coerce(attr: &str, v: Value) -> Value {
    if attr.eq_ignore_ascii_case("active")
        && let Value::String(s) = &v
    {
        match s.to_ascii_lowercase().as_str() {
            "true" => return Value::Bool(true),
            "false" => return Value::Bool(false),
            _ => {}
        }
    }
    v
}

fn same_element(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => match (get_ci(x, "value"), get_ci(y, "value")) {
            (Some(p), Some(q)) => p == q,
            _ => x == y,
        },
        _ => a == b,
    }
}

fn merge_into(target: &mut Value, v: &Value) {
    match (target, v) {
        (Value::Object(t), Value::Object(s)) => {
            for (k, val) in s {
                let existing = key_ci(t, k).unwrap_or_else(|| k.clone());
                t.insert(existing, val.clone());
            }
        }
        (t, s) => *t = s.clone(),
    }
}

fn read_only(attr: &str) -> bool {
    ["id", "meta", "groups"]
        .iter()
        .any(|r| attr.eq_ignore_ascii_case(r))
}

/// Applies PATCH operations to the JSON form of a resource.
pub fn apply_patch(resource: &mut Value, ops: &[PatchOp]) -> Result<(), ScimError> {
    let obj = resource
        .as_object_mut()
        .ok_or_else(|| ScimError::new(500, "", "resource is not an object"))?;
    for op in ops {
        match (op.op.as_str(), &op.path) {
            ("add" | "replace", None) => {
                let Some(Value::Object(v)) = &op.value else {
                    return Err(ScimError::new(
                        400,
                        "invalidValue",
                        "a path-less operation needs an object value",
                    ));
                };
                for (k, val) in v {
                    if read_only(k) {
                        return Err(ScimError::new(
                            400,
                            "mutability",
                            format!("`{k}` is read only"),
                        ));
                    }
                    let path = Path {
                        attr: k.clone(),
                        filter: None,
                        sub: None,
                    };
                    set(obj, &path, coerce(k, val.clone()), op.op == "add")?;
                }
            }
            ("add" | "replace", Some(p)) => {
                let path = parse_path(p)?;
                if read_only(&path.attr) {
                    return Err(ScimError::new(
                        400,
                        "mutability",
                        format!("`{}` is read only", path.attr),
                    ));
                }
                let v = op.value.clone().ok_or_else(|| {
                    ScimError::new(400, "invalidValue", "the operation needs a value")
                })?;
                set(obj, &path, coerce(&path.attr, v), op.op == "add")?;
            }
            ("remove", Some(p)) => {
                let path = parse_path(p)?;
                if read_only(&path.attr) {
                    return Err(ScimError::new(
                        400,
                        "mutability",
                        format!("`{}` is read only", path.attr),
                    ));
                }
                remove(obj, &path, op.value.as_ref())?;
            }
            ("remove", None) => {
                return Err(ScimError::new(400, "noTarget", "remove needs a path"));
            }
            (other, _) => {
                return Err(ScimError::new(
                    400,
                    "invalidSyntax",
                    format!("unknown operation `{other}`"),
                ));
            }
        }
    }
    Ok(())
}

fn set(obj: &mut Map<String, Value>, path: &Path, v: Value, add: bool) -> Result<(), ScimError> {
    let key = key_ci(obj, &path.attr).unwrap_or_else(|| path.attr.clone());
    match (&path.filter, &path.sub) {
        (None, None) => {
            if add && let Some(Value::Array(existing)) = obj.get_mut(&key) {
                let items = match v {
                    Value::Array(a) => a,
                    one => vec![one],
                };
                for it in items {
                    if let Some(slot) = existing.iter_mut().find(|e| same_element(e, &it)) {
                        *slot = it;
                    } else {
                        existing.push(it);
                    }
                }
                return Ok(());
            }
            if add
                && let (Some(Value::Object(_)), Value::Object(_)) = (obj.get(&key), &v)
                && let Some(t) = obj.get_mut(&key)
            {
                merge_into(t, &v);
                return Ok(());
            }
            obj.insert(key, v);
        }
        (None, Some(sub)) => {
            let entry = obj.entry(key).or_insert_with(|| json!({}));
            match entry {
                Value::Object(o) => {
                    let k = key_ci(o, sub).unwrap_or_else(|| sub.clone());
                    o.insert(k, v);
                }
                Value::Array(a) => {
                    for e in a.iter_mut() {
                        if let Value::Object(o) = e {
                            let k = key_ci(o, sub).unwrap_or_else(|| sub.clone());
                            o.insert(k, v.clone());
                        }
                    }
                }
                _ => {
                    return Err(ScimError::new(
                        400,
                        "invalidPath",
                        "cannot set a sub-attribute here",
                    ));
                }
            }
        }
        (Some(f), sub) => {
            let Some(Value::Array(a)) = obj.get_mut(&key) else {
                return Err(ScimError::new(
                    400,
                    "noTarget",
                    format!("`{}` has no matching value", path.attr),
                ));
            };
            let mut hit = false;
            for e in a.iter_mut().filter(|e| f.matches(e)) {
                hit = true;
                match sub {
                    Some(s) => {
                        if let Value::Object(o) = e {
                            let k = key_ci(o, s).unwrap_or_else(|| s.clone());
                            o.insert(k, v.clone());
                        }
                    }
                    None => merge_into(e, &v),
                }
            }
            if !hit {
                if add && sub.is_none() {
                    a.push(v);
                } else {
                    return Err(ScimError::new(
                        400,
                        "noTarget",
                        "the value path matches nothing",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn remove(
    obj: &mut Map<String, Value>,
    path: &Path,
    value: Option<&Value>,
) -> Result<(), ScimError> {
    let Some(key) = key_ci(obj, &path.attr) else {
        return Ok(());
    };
    match (&path.filter, &path.sub) {
        (None, None) => {
            if let (Some(Value::Array(a)), Some(v)) = (obj.get_mut(&key), value) {
                let drop: Vec<Value> = match v {
                    Value::Array(x) => x.clone(),
                    one => vec![one.clone()],
                };
                a.retain(|e| !drop.iter().any(|d| same_element(e, d)));
            } else {
                obj.remove(&key);
            }
        }
        (None, Some(sub)) => match obj.get_mut(&key) {
            Some(Value::Object(o)) => {
                if let Some(k) = key_ci(o, sub) {
                    o.remove(&k);
                }
            }
            Some(Value::Array(a)) => {
                for e in a {
                    if let Value::Object(o) = e
                        && let Some(k) = key_ci(o, sub)
                    {
                        o.remove(&k);
                    }
                }
            }
            _ => {}
        },
        (Some(f), None) => {
            if let Some(Value::Array(a)) = obj.get_mut(&key) {
                a.retain(|e| !f.matches(e));
            }
        }
        (Some(f), Some(sub)) => {
            if let Some(Value::Array(a)) = obj.get_mut(&key) {
                for e in a.iter_mut().filter(|e| f.matches(e)) {
                    if let Value::Object(o) = e
                        && let Some(k) = key_ci(o, sub)
                    {
                        o.remove(&k);
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> Value {
        json!({
            "id": "u1", "userName": "Alice@Example.com", "active": true, "externalId": "ext-1",
            "name": {"givenName": "Alice", "familyName": "Liddell"},
            "emails": [
                {"value": "alice@example.com", "type": "work", "primary": true},
                {"value": "a@home.example", "type": "home"}
            ],
            "meta": {"created": "2024-01-01T00:00:00Z"}
        })
    }

    fn m(f: &str) -> bool {
        parse_filter(f).unwrap().matches(&user())
    }

    #[test]
    fn filters_match_per_rfc() {
        assert!(m(r#"userName eq "alice@example.com""#));
        assert!(m(r#"USERNAME EQ "ALICE@EXAMPLE.COM""#));
        assert!(!m(r#"externalId eq "EXT-1""#));
        assert!(m(r#"userName co "example""#));
        assert!(m(r#"userName sw "alice" and active eq true"#));
        assert!(m(r#"userName ew ".com" or active eq false"#));
        assert!(m(r#"not (active eq false)"#));
        assert!(m(r#"name.familyName eq "liddell""#));
        assert!(m(r#"emails.value ew "home.example""#));
        assert!(m(r#"emails[type eq "work" and primary eq true]"#));
        assert!(!m(r#"emails[type eq "home" and primary eq true]"#));
        assert!(m(r#"emails[type eq "home"] and externalId pr"#));
        assert!(m("externalId pr"));
        assert!(!m("title pr"));
        assert!(m(r#"title ne "x""#));
        assert!(m(r#"meta.created gt "2023-12-31T00:00:00Z""#));
        assert!(m(
            r#"(userName eq "nobody" or userName eq "alice@example.com") and active eq true"#
        ));
    }

    #[test]
    fn bad_filters_are_rejected() {
        for f in [
            "",
            "userName",
            "userName eq",
            "userName foo \"x\"",
            "(userName eq \"x\"",
            "not userName eq \"x\"",
            "a eq \"x\" b",
        ] {
            let e = parse_filter(f).unwrap_err();
            assert_eq!(e.scim_type, "invalidFilter", "{f}");
        }
    }

    #[test]
    fn patch_replace_add_remove() {
        let mut u = user();
        let ops = parse_patch(&json!({"Operations": [
            {"op": "Replace", "path": "active", "value": "False"},
            {"op": "replace", "path": "name.givenName", "value": "Alicia"},
            {"op": "add", "path": "emails", "value": [{"value": "new@example.com", "type": "other"}]},
            {"op": "remove", "path": "emails[type eq \"home\"]"},
            {"op": "add", "value": {"displayName": "Alice L"}},
            {"op": "replace", "path": "emails[type eq \"work\"].primary", "value": false}
        ]})).unwrap();
        apply_patch(&mut u, &ops).unwrap();
        assert_eq!(u["active"], json!(false));
        assert_eq!(u["name"]["givenName"], json!("Alicia"));
        assert_eq!(u["displayName"], json!("Alice L"));
        let emails = u["emails"].as_array().unwrap();
        assert_eq!(emails.len(), 2);
        assert_eq!(emails[0]["primary"], json!(false));
        assert_eq!(emails[1]["value"], json!("new@example.com"));
    }

    #[test]
    fn patch_group_members() {
        let mut g = json!({"id": "g1", "displayName": "core", "members": [{"value": "u1"}]});
        let ops = parse_patch(&json!({"Operations": [
            {"op": "add", "path": "members", "value": [{"value": "u2"}, {"value": "u1"}]},
            {"op": "remove", "path": "members[value eq \"u1\"]"},
        ]}))
        .unwrap();
        apply_patch(&mut g, &ops).unwrap();
        assert_eq!(g["members"], json!([{"value": "u2"}]));
        let ops = parse_patch(&json!({"Operations": [
            {"op": "remove", "path": "members", "value": [{"value": "u2"}]},
        ]}))
        .unwrap();
        apply_patch(&mut g, &ops).unwrap();
        assert_eq!(g["members"], json!([]));
        let ops =
            parse_patch(&json!({"Operations": [{"op": "replace", "path": "id", "value": "x"}]}))
                .unwrap();
        assert_eq!(
            apply_patch(&mut g, &ops).unwrap_err().scim_type,
            "mutability"
        );
        let ops = parse_patch(&json!({"Operations": [{"op": "replace", "path": "members[value eq \"zz\"].display", "value": "x"}]})).unwrap();
        assert_eq!(apply_patch(&mut g, &ops).unwrap_err().scim_type, "noTarget");
        assert!(parse_patch(&json!({"Operations": []})).is_err());
    }
}
