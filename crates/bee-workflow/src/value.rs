//! Expression values: the JSON-like data model of contexts and expression results.
//!
//! Objects keep insertion order and look keys up case-insensitively, as GitHub does. Arrays and
//! objects are shared (`Arc`), because GitHub compares them by identity: two references to the
//! same context object are equal, two equal-looking literals are not.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::sync::Arc;

use crate::yaml::{Node, NodeKind, Scalar};

/// A value produced by an expression or held in a context.
#[derive(Clone, Debug)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A double-precision number.
    Number(f64),
    /// A string.
    String(String),
    /// An array, compared by identity.
    Array(Arc<Vec<Value>>),
    /// An object, compared by identity.
    Object(Arc<Object>),
}

/// An insertion-ordered map with case-insensitive keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object {
    entries: Vec<(String, Value)>,
}

/// Case-insensitive string equality (Unicode simple case folding through lowercase).
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// Case-insensitive ordering: compares the upper-cased characters (ordinal ignore case).
pub fn cmp_ignore_case(a: &str, b: &str) -> Ordering {
    a.chars()
        .flat_map(char::to_uppercase)
        .cmp(b.chars().flat_map(char::to_uppercase))
}

impl Object {
    /// An empty object.
    pub fn new() -> Self {
        Object::default()
    }

    /// Sets a key, replacing an existing entry that matches case-insensitively in place.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        let key = key.into();
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|(k, _)| eq_ignore_case(k, &key))
        {
            slot.1 = value;
        } else {
            self.entries.push((key, value));
        }
    }

    /// Looks a key up case-insensitively.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(k, _)| eq_ignore_case(k, key))
            .map(|(_, v)| v)
    }

    /// Whether the key is present (case-insensitively).
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the object is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl FromIterator<(String, Value)> for Object {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        let mut o = Object::new();
        for (k, v) in iter {
            o.insert(k, v);
        }
        o
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Number(n)
    }
}

impl From<Object> for Value {
    fn from(o: Object) -> Self {
        Value::Object(Arc::new(o))
    }
}

impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(Arc::new(v))
    }
}

/// Structural equality (used by tests and by matrix matching, not by the `==` operator).
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va == vb)
            }
            _ => false,
        }
    }
}

/// The kinds of value, for messages and coercion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Null.
    Null,
    /// Boolean.
    Bool,
    /// Number.
    Number,
    /// String.
    String,
    /// Array.
    Array,
    /// Object.
    Object,
}

impl Value {
    /// Builds an object value from pairs.
    pub fn object<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::from(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v))
                .collect::<Object>(),
        )
    }

    /// Builds an array value.
    pub fn array(items: impl IntoIterator<Item = Value>) -> Value {
        Value::from(items.into_iter().collect::<Vec<_>>())
    }

    /// The kind of this value.
    pub fn kind(&self) -> Kind {
        match self {
            Value::Null => Kind::Null,
            Value::Bool(_) => Kind::Bool,
            Value::Number(_) => Kind::Number,
            Value::String(_) => Kind::String,
            Value::Array(_) => Kind::Array,
            Value::Object(_) => Kind::Object,
        }
    }

    /// Whether this is an array or object.
    pub fn is_collection(&self) -> bool {
        matches!(self, Value::Array(_) | Value::Object(_))
    }

    /// GitHub truthiness: `false`, `0`, `-0`, `NaN`, `""` and `null` are falsy.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => !(*n == 0.0 || n.is_nan()),
            Value::String(s) => !s.is_empty(),
            Value::Array(_) | Value::Object(_) => true,
        }
    }

    /// Converts to a number with GitHub's coercion rules.
    pub fn to_number(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Bool(b) => f64::from(u8::from(*b)),
            Value::Number(n) => *n,
            Value::String(s) => parse_number(s),
            Value::Array(_) | Value::Object(_) => f64::NAN,
        }
    }

    /// Converts to a string with GitHub's coercion rules (`Array` and `Object` for collections).
    pub fn to_display_string(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => format_number(*n),
            Value::String(s) => s.clone(),
            Value::Array(_) => "Array".to_owned(),
            Value::Object(_) => "Object".to_owned(),
        }
    }

    /// Looks up an object key case-insensitively.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(o) => o.get(key),
            _ => None,
        }
    }

    /// The string, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The object, if this is an object.
    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// The array, if this is an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Loose equality (`==`): numbers after coercion when kinds differ, strings
    /// case-insensitively, collections by identity.
    pub fn loose_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::String(a), Value::String(b)) => eq_ignore_case(a, b),
            (Value::Array(a), Value::Array(b)) => Arc::ptr_eq(a, b),
            (Value::Object(a), Value::Object(b)) => Arc::ptr_eq(a, b),
            _ => {
                if self.is_collection() || other.is_collection() {
                    return false;
                }
                let (a, b) = (self.to_number(), other.to_number());
                a == b
            }
        }
    }

    /// Relational comparison; `None` when the operands are unordered (NaN, collections).
    pub fn loose_cmp(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::String(a), Value::String(b)) => Some(cmp_ignore_case(a, b)),
            _ => {
                if self.is_collection() || other.is_collection() {
                    return None;
                }
                self.to_number().partial_cmp(&other.to_number())
            }
        }
    }

    /// Serialises as indented JSON (two spaces), the format of `toJSON`.
    pub fn to_json_pretty(&self) -> String {
        let mut out = String::new();
        write_json(self, &mut out, Some(0));
        out
    }

    /// Serialises as compact JSON.
    pub fn to_json_compact(&self) -> String {
        let mut out = String::new();
        write_json(self, &mut out, None);
        out
    }

    /// Converts a YAML node to a value (core schema; mappings keep their order).
    pub fn from_yaml(node: &Node) -> Value {
        match &node.kind {
            NodeKind::Scalar(..) => match node.scalar() {
                Some(Scalar::Null) | None => Value::Null,
                Some(Scalar::Bool(b)) => Value::Bool(b),
                Some(Scalar::Number(n)) => Value::Number(n),
                Some(Scalar::Str(s)) => Value::String(s.to_owned()),
            },
            NodeKind::Seq(items) => Value::array(items.iter().map(Value::from_yaml)),
            NodeKind::Map(entries) => Value::object(entries.iter().map(|(k, v)| {
                (
                    k.as_str().unwrap_or_default().to_owned(),
                    Value::from_yaml(v),
                )
            })),
        }
    }
}

/// Parses a string as a number: trimmed, empty is 0, `0x` hex, `0o` octal, decimals with an
/// optional sign and exponent, `Infinity`/`NaN`; anything else is NaN.
pub fn parse_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        "NaN" => return f64::NAN,
        _ => {}
    }
    if let Some(h) = t.strip_prefix("0x") {
        return parse_radix(h, 16);
    }
    if let Some(o) = t.strip_prefix("0o") {
        return parse_radix(o, 8);
    }
    if is_decimal(t) {
        t.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

fn parse_radix(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() || digits.len() > 16 {
        return f64::NAN;
    }
    let mut v: f64 = 0.0;
    for c in digits.chars() {
        let Some(d) = c.to_digit(radix) else {
            return f64::NAN;
        };
        v = v * f64::from(radix) + f64::from(d);
    }
    v
}

/// `[+-]? (digits (. digits?)? | . digits) ([eE] [+-]? digits)?`
fn is_decimal(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i = 1;
    }
    let s = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - s;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - f;
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return false;
        }
    }
    i == b.len()
}

/// Formats a number like GitHub: 15 significant digits, integers without a fraction,
/// scientific notation (`1E+21`, `1E-07`) outside `1e-5 .. 1e15`.
pub fn format_number(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_owned();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if n == 0.0 {
        return if n.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    // Round to 15 significant digits.
    let sci = format!("{:.14e}", n.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let sign = if n < 0.0 { "-" } else { "" };
    if (-5 < exp) && (exp < 15) {
        let mut out = String::from(sign);
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(digits);
                out.push_str(&"0".repeat(int_len - digits.len()));
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exp - 1) as usize));
            out.push_str(digits);
        }
        out
    } else {
        let mut out = String::from(sign);
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(
            out,
            "E{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.unsigned_abs()
        );
        out
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_json(v: &Value, out: &mut String, indent: Option<usize>) {
    let newline = |out: &mut String, level: usize| {
        out.push('\n');
        for _ in 0..level {
            out.push_str("  ");
        }
    };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_finite() {
                out.push_str(&format_number(*n));
            } else {
                // JSON has no NaN or Infinity; emit them as strings.
                write_json_string(&format_number(*n), out);
            }
        }
        Value::String(s) => write_json_string(s, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let Some(level) = indent {
                    newline(out, level + 1);
                }
                write_json(item, out, indent.map(|l| l + 1));
            }
            if let Some(level) = indent {
                newline(out, level);
            }
            out.push(']');
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in obj.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let Some(level) = indent {
                    newline(out, level + 1);
                }
                write_json_string(k, out);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_json(item, out, indent.map(|l| l + 1));
            }
            if let Some(level) = indent {
                newline(out, level);
            }
            out.push('}');
        }
    }
}

/// A JSON parse error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    /// Byte offset of the problem.
    pub offset: usize,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid JSON at offset {}: {}",
            self.offset, self.message
        )
    }
}

impl std::error::Error for JsonError {}

/// Parses JSON text into a value (used by `fromJSON`).
pub fn parse_json(text: &str) -> Result<Value, JsonError> {
    let mut p = JsonParser {
        b: text.as_bytes(),
        i: 0,
        depth: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("unexpected trailing characters"));
    }
    Ok(v)
}

struct JsonParser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl JsonParser<'_> {
    fn err(&self, message: &str) -> JsonError {
        JsonError {
            offset: self.i,
            message: message.to_owned(),
        }
    }

    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn lit(&mut self, word: &str, v: Value) -> Result<Value, JsonError> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("unexpected token"))
        }
    }

    fn value(&mut self) -> Result<Value, JsonError> {
        self.depth += 1;
        if self.depth > 256 {
            return Err(self.err("nesting is too deep"));
        }
        let r = match self.b.get(self.i) {
            None => Err(self.err("unexpected end of input")),
            Some(b'n') => self.lit("null", Value::Null),
            Some(b't') => self.lit("true", Value::Bool(true)),
            Some(b'f') => self.lit("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        items.push(self.value()?);
                        self.ws();
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b']') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err(self.err("expected ',' or ']'")),
                        }
                    }
                }
                Ok(Value::array(items))
            }
            Some(b'{') => {
                self.i += 1;
                let mut obj = Object::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        if self.b.get(self.i) != Some(&b'"') {
                            return Err(self.err("expected a string key"));
                        }
                        let k = self.string()?;
                        self.ws();
                        if self.b.get(self.i) != Some(&b':') {
                            return Err(self.err("expected ':'"));
                        }
                        self.i += 1;
                        self.ws();
                        let v = self.value()?;
                        obj.insert(k, v);
                        self.ws();
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b'}') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err(self.err("expected ',' or '}'")),
                        }
                    }
                }
                Ok(Value::from(obj))
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        };
        self.depth -= 1;
        r
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while matches!(p.b.get(p.i), Some(b'0'..=b'9')) {
                p.i += 1;
            }
            p.i - s
        };
        if digits(self) == 0 {
            return Err(self.err("expected digits"));
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return Err(self.err("expected digits after '.'"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return Err(self.err("expected exponent digits"));
            }
        }
        let text =
            std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("bad number"))?;
        text.parse::<f64>()
            .map(Value::Number)
            .map_err(|_| self.err("bad number"))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let mut v = 0;
        for _ in 0..4 {
            let d = self
                .b
                .get(self.i)
                .and_then(|c| (*c as char).to_digit(16))
                .ok_or_else(|| self.err("bad \\u escape"))?;
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while let Some(&c) = self.b.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.b[start..self.i])
                    .map_err(|_| self.err("invalid UTF-8"))?,
            );
            match self.b.get(self.i) {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let e = *self.b.get(self.i).ok_or_else(|| self.err("bad escape"))?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                if self.b.get(self.i) == Some(&b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(self.err("bad surrogate pair"));
                                    }
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    return Err(self.err("lone surrogate"));
                                }
                            } else {
                                hi
                            };
                            out.push(char::from_u32(code).ok_or_else(|| self.err("bad escape"))?);
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formatting_matches_g15() {
        assert_eq!(format_number(1.0), "1");
        assert_eq!(format_number(3.1), "3.1");
        assert_eq!(format_number(-2.99e-2), "-0.0299");
        assert_eq!(format_number(0.1 + 0.2), "0.3");
        assert_eq!(format_number(1e21), "1E+21");
        assert_eq!(format_number(123456789012345.0), "123456789012345");
        assert_eq!(format_number(1e15), "1E+15");
        assert_eq!(format_number(0.0001), "0.0001");
        assert_eq!(format_number(0.00001), "1E-05");
        assert_eq!(format_number(255.0), "255");
        assert_eq!(format_number(f64::NAN), "NaN");
        assert_eq!(format_number(f64::INFINITY), "Infinity");
        assert_eq!(format_number(1.5e300), "1.5E+300");
    }

    #[test]
    fn string_to_number_coercion() {
        assert_eq!(parse_number(""), 0.0);
        assert_eq!(parse_number("  42 "), 42.0);
        assert_eq!(parse_number("0xff"), 255.0);
        assert_eq!(parse_number("0o17"), 15.0);
        assert_eq!(parse_number("-1.5e2"), -150.0);
        assert!(parse_number("abc").is_nan());
        assert!(parse_number("1.2.3").is_nan());
    }

    #[test]
    fn truthiness() {
        assert!(!Value::Null.is_truthy());
        assert!(!Value::from("").is_truthy());
        assert!(!Value::Number(f64::NAN).is_truthy());
        assert!(!Value::Number(-0.0).is_truthy());
        assert!(Value::from("false").is_truthy());
        assert!(Value::array([]).is_truthy());
    }

    #[test]
    fn loose_equality() {
        assert!(Value::Null.loose_eq(&Value::from("")));
        assert!(Value::from("ABC").loose_eq(&Value::from("abc")));
        assert!(Value::Bool(true).loose_eq(&Value::Number(1.0)));
        assert!(Value::from("1").loose_eq(&Value::Number(1.0)));
        assert!(!Value::Number(f64::NAN).loose_eq(&Value::Number(f64::NAN)));
        let a = Value::array([Value::Null]);
        assert!(a.loose_eq(&a.clone()));
        assert!(!a.loose_eq(&Value::array([Value::Null])));
        assert!(!a.loose_eq(&Value::from("Array")));
    }

    #[test]
    fn json_round_trip_and_pretty_print() {
        let v = parse_json(r#"{"a":[1,2.5,"x\n"],"b":{"c":null,"d":true},"e":{}}"#).unwrap();
        assert_eq!(
            v.to_json_pretty(),
            "{\n  \"a\": [\n    1,\n    2.5,\n    \"x\\n\"\n  ],\n  \"b\": {\n    \"c\": null,\n    \"d\": true\n  },\n  \"e\": {}\n}"
        );
        assert_eq!(parse_json(&v.to_json_compact()).unwrap(), v);
        assert!(parse_json("{").is_err());
        assert!(parse_json("[1,]").is_err());
        assert_eq!(parse_json("\"\\ud83d\\ude00\"").unwrap(), Value::from("😀"));
    }

    #[test]
    fn objects_are_case_insensitive_and_ordered() {
        let mut o = Object::new();
        o.insert("Zeta", Value::Null);
        o.insert("alpha", Value::Bool(true));
        o.insert("ZETA", Value::Bool(false));
        assert_eq!(o.len(), 2);
        assert_eq!(o.get("zeta"), Some(&Value::Bool(false)));
        assert_eq!(o.iter().next().unwrap().0, "Zeta");
    }
}
