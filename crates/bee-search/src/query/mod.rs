//! The query syntax shared by code and forge search: tokens, boolean structure and errors.
//!
//! A query is a sequence of terms separated by whitespace. Juxtaposition and `AND` mean
//! conjunction, `OR` disjunction (lower precedence), `NOT` or a leading `-` negation, and
//! parentheses group. A term is a bare word, a `"quoted phrase"` (with `\"` and `\\`
//! escapes), a `/regex/` where the grammar allows it, or `key:value` where `value` is any of
//! those. Grammars decide which keys are qualifiers ([`LexOptions::is_qualifier`]).
//!
//! Parenthesis rule: `(` opens a group only at the start of a term and `)` closes one only
//! at the end of a term while a group is open and the term has no unmatched `(` of its own,
//! so code such as `charge_card(` or `foo()` stays literal.

pub mod code;

use std::fmt;

use bee_core::{BeeError, PayloadValue, Reason};

/// A half-open byte range in the query string.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    /// First byte.
    pub start: u32,
    /// One past the last byte.
    pub end: u32,
}

impl Span {
    /// A span from `usize` offsets.
    pub fn new(start: usize, end: usize) -> Span {
        Span {
            start: start as u32,
            end: end as u32,
        }
    }
}

/// A query error with its position (mirrors `bee.v1.SearchQueryError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    /// Human-readable explanation; not the contract.
    pub message: String,
    /// Stable snake_case subcode, e.g. `unknown_qualifier`, `bad_regex`, `unbalanced_paren`.
    pub subcode: &'static str,
    /// Where in the query.
    pub span: Span,
}

impl QueryError {
    /// A new error.
    pub fn new(subcode: &'static str, span: Span, message: impl Into<String>) -> QueryError {
        QueryError {
            message: message.into(),
            subcode,
            span,
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} at {}..{}: {}",
            self.subcode, self.span.start, self.span.end, self.message
        )
    }
}

/// The payload subcode every query syntax error carries (SPEC §12.2, search.proto).
pub const BAD_QUERY: &str = "bad_query";

/// Converts query errors into the client-visible error: `INVALID_ARGUMENT` with subcode
/// `bad_query` and an `errors` list of `{subcode, message, start, end}`. A regex too large
/// to compile is `TOO_LARGE` instead.
pub fn to_bee_error(errors: &[QueryError]) -> BeeError {
    let too_large = errors.iter().any(|e| e.subcode == "regex_too_large");
    let first = errors
        .first()
        .map_or_else(|| "invalid query".to_string(), ToString::to_string);
    let list = errors
        .iter()
        .map(|e| {
            let mut m = std::collections::BTreeMap::new();
            m.insert("subcode".to_string(), PayloadValue::from(e.subcode));
            m.insert("message".to_string(), PayloadValue::from(e.message.clone()));
            m.insert(
                "start".to_string(),
                PayloadValue::Int(i64::from(e.span.start)),
            );
            m.insert("end".to_string(), PayloadValue::Int(i64::from(e.span.end)));
            PayloadValue::Map(m)
        })
        .collect();
    let base = if too_large {
        BeeError::new(Reason::TooLarge, first).with_payload("limit", "regex_size")
    } else {
        BeeError::new(Reason::InvalidArgument, first).with_payload("subcode", BAD_QUERY)
    };
    base.with_payload("errors", PayloadValue::List(list))
}

/// How a term's value was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueKind {
    /// A bare word.
    Bare,
    /// A `"quoted phrase"` (escapes removed).
    Quoted,
    /// A `/regex/` (slashes removed, `\/` unescaped).
    Regex,
}

/// One term as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atom {
    /// The qualifier key, lower-cased, if any.
    pub key: Option<String>,
    /// The value, with quoting removed.
    pub value: String,
    /// How the value was written.
    pub kind: ValueKind,
    /// The whole term, `-` and key included.
    pub span: Span,
    /// Just the value.
    pub value_span: Span,
}

/// A boolean expression over terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr<T> {
    /// A term.
    Term(T),
    /// Negation.
    Not(Box<Expr<T>>),
    /// Every child holds.
    And(Vec<Expr<T>>),
    /// Some child holds.
    Or(Vec<Expr<T>>),
}

impl<T> Expr<T> {
    /// Maps terms, collecting every error rather than stopping at the first. `negated` tells
    /// the mapper whether the term sits under an odd number of negations.
    pub fn try_map<U, E>(
        self,
        f: &mut dyn FnMut(T, bool) -> std::result::Result<U, E>,
        errors: &mut Vec<E>,
    ) -> Option<Expr<U>> {
        self.try_map_inner(f, errors, false)
    }

    fn try_map_inner<U, E>(
        self,
        f: &mut dyn FnMut(T, bool) -> std::result::Result<U, E>,
        errors: &mut Vec<E>,
        negated: bool,
    ) -> Option<Expr<U>> {
        match self {
            Expr::Term(t) => match f(t, negated) {
                Ok(u) => Some(Expr::Term(u)),
                Err(e) => {
                    errors.push(e);
                    None
                }
            },
            Expr::Not(inner) => inner
                .try_map_inner(f, errors, !negated)
                .map(|e| Expr::Not(Box::new(e))),
            Expr::And(v) => {
                let out: Vec<Option<Expr<U>>> = v
                    .into_iter()
                    .map(|e| e.try_map_inner(f, errors, negated))
                    .collect();
                out.into_iter().collect::<Option<Vec<_>>>().map(Expr::And)
            }
            Expr::Or(v) => {
                let out: Vec<Option<Expr<U>>> = v
                    .into_iter()
                    .map(|e| e.try_map_inner(f, errors, negated))
                    .collect();
                out.into_iter().collect::<Option<Vec<_>>>().map(Expr::Or)
            }
        }
    }

    /// Every term, in order.
    pub fn terms(&self) -> Vec<&T> {
        let mut out = Vec::new();
        self.collect_terms(&mut out);
        out
    }

    fn collect_terms<'a>(&'a self, out: &mut Vec<&'a T>) {
        match self {
            Expr::Term(t) => out.push(t),
            Expr::Not(e) => e.collect_terms(out),
            Expr::And(v) | Expr::Or(v) => v.iter().for_each(|e| e.collect_terms(out)),
        }
    }

    /// Evaluates with a term predicate. Children are evaluated in order and short-circuit.
    pub fn eval(&self, f: &mut dyn FnMut(&T) -> bool) -> bool {
        match self {
            Expr::Term(t) => f(t),
            Expr::Not(e) => !e.eval(f),
            Expr::And(v) => v.iter().all(|e| e.eval(f)),
            Expr::Or(v) => v.iter().any(|e| e.eval(f)),
        }
    }

    /// Renders the expression canonically with a term renderer: `AND` is juxtaposition,
    /// `OR` groups are parenthesized when nested, a negated term is `-term`.
    pub fn render(&self, term: &dyn Fn(&T) -> String) -> String {
        self.render_prec(term, 0)
    }

    fn render_prec(&self, term: &dyn Fn(&T) -> String, prec: u8) -> String {
        match self {
            Expr::Term(t) => term(t),
            Expr::Not(e) => match e.as_ref() {
                Expr::Term(t) => format!("-{}", term(t)),
                other => format!("NOT ({})", other.render_prec(term, 0)),
            },
            Expr::And(v) => {
                let s = v
                    .iter()
                    .map(|e| e.render_prec(term, 2))
                    .collect::<Vec<_>>()
                    .join(" ");
                if prec > 2 { format!("({s})") } else { s }
            }
            Expr::Or(v) => {
                let s = v
                    .iter()
                    .map(|e| e.render_prec(term, 1))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                if prec >= 1 { format!("({s})") } else { s }
            }
        }
    }
}

/// Renders a value so that it lexes back to itself.
pub fn quote_value(value: &str, kind: ValueKind) -> String {
    match kind {
        ValueKind::Regex => format!("/{}/", value.replace('/', "\\/")),
        _ => {
            let needs = value.is_empty()
                || value.chars().any(|c| c.is_whitespace() || c == '"')
                || value.starts_with('(')
                || value.starts_with('-')
                || (value.starts_with('/') && read_regex(value, 0).is_some())
                || value.ends_with(')')
                || matches!(value, "AND" | "OR" | "NOT");
            let qualifier_like = value.split_once(':').is_some_and(|(k, v)| {
                !v.is_empty()
                    && k.bytes().next().is_some_and(is_key_start)
                    && k.bytes().all(is_key_char)
            });
            if needs || kind == ValueKind::Quoted && qualifier_like {
                format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                value.to_string()
            }
        }
    }
}

/// Lexer options a grammar supplies.
pub struct LexOptions<'a> {
    /// Whether `/regex/` values are recognized.
    pub regex: bool,
    /// Whether a lower-cased key is a qualifier of the grammar. Keys it rejects stay part
    /// of a literal term (code search) unless `strict_keys` is set.
    pub is_qualifier: &'a dyn Fn(&str) -> bool,
    /// Report `key:value` terms with an unknown key as `unknown_qualifier` instead of
    /// treating them as text (forge grammars).
    pub strict_keys: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    LParen(Span),
    RParen(Span),
    And(Span),
    Or(Span),
    Not(Span),
    /// An atom; `negated` from a leading `-`.
    Atom(Atom, bool),
}

impl Tok {
    fn span(&self) -> Span {
        match self {
            Tok::LParen(s) | Tok::RParen(s) | Tok::And(s) | Tok::Or(s) | Tok::Not(s) => *s,
            Tok::Atom(a, _) => a.span,
        }
    }
}

fn is_key_start(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

fn is_key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Reads a `"..."` starting at `pos` (on the quote). Returns (value, end offset).
fn read_quoted(s: &str, pos: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = pos + 1;
    while i < b.len() {
        match b[i] {
            b'\\' if i + 1 < b.len() && (b[i + 1] == b'"' || b[i + 1] == b'\\') => {
                out.push(b[i + 1]);
                i += 2;
            }
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), i + 1)),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

/// Reads a `/.../` starting at `pos` (on the slash): the closing slash must be followed by
/// whitespace, `)` or the end. Returns (pattern, end offset).
fn read_regex(s: &str, pos: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = pos + 1;
    while i < b.len() {
        match b[i] {
            b'\\' if i + 1 < b.len() && b[i + 1] == b'/' => {
                out.push(b'/');
                i += 2;
            }
            b'\\' if i + 1 < b.len() => {
                out.push(b'\\');
                out.push(b[i + 1]);
                i += 2;
            }
            b'/' => {
                let next = b.get(i + 1).copied();
                if !out.is_empty() && next.is_none_or(|c| c.is_ascii_whitespace() || c == b')') {
                    return Some((String::from_utf8_lossy(&out).into_owned(), i + 1));
                }
                out.push(b'/');
                i += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

fn lex(s: &str, opts: &LexOptions<'_>, errors: &mut Vec<QueryError>) -> Vec<Tok> {
    let b = s.as_bytes();
    let mut toks = Vec::new();
    let mut depth = 0usize;
    let mut pos = 0usize;
    while pos < b.len() {
        let c = b[pos];
        if c.is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        if c == b'(' {
            toks.push(Tok::LParen(Span::new(pos, pos + 1)));
            depth += 1;
            pos += 1;
            continue;
        }
        if c == b')' && depth > 0 {
            toks.push(Tok::RParen(Span::new(pos, pos + 1)));
            depth -= 1;
            pos += 1;
            continue;
        }
        let start = pos;
        let mut negated = false;
        if c == b'-'
            && b.get(pos + 1)
                .is_some_and(|n| !n.is_ascii_whitespace() && *n != b'-')
        {
            negated = true;
            pos += 1;
        }
        // A qualifier key?
        let mut key = None;
        if pos < b.len() && is_key_start(b[pos]) {
            let mut j = pos;
            while j < b.len() && is_key_char(b[j]) {
                j += 1;
            }
            if j < b.len()
                && b[j] == b':'
                && b.get(j + 1)
                    .is_some_and(|n| !n.is_ascii_whitespace() && *n != b')')
            {
                let k = s[pos..j].to_ascii_lowercase();
                if (opts.is_qualifier)(&k) {
                    key = Some(k);
                    pos = j + 1;
                } else if opts.strict_keys && b.get(j + 1) != Some(&b':') {
                    errors.push(QueryError::new(
                        "unknown_qualifier",
                        Span::new(pos, j),
                        format!("unknown qualifier {:?}", &s[pos..j]),
                    ));
                    key = Some(k);
                    pos = j + 1;
                }
            }
        }
        let value_start = pos;
        let (value, kind, end) = if b.get(pos) == Some(&b'"') {
            match read_quoted(s, pos) {
                Some((v, e)) => (v, ValueKind::Quoted, e),
                None => {
                    errors.push(QueryError::new(
                        "unterminated_quote",
                        Span::new(pos, b.len()),
                        "missing closing quote",
                    ));
                    (s[pos + 1..].to_string(), ValueKind::Quoted, b.len())
                }
            }
        } else if opts.regex && b.get(pos) == Some(&b'/') {
            match read_regex(s, pos) {
                Some((v, e)) => (v, ValueKind::Regex, e),
                None => {
                    let e = bare_end(b, pos);
                    (s[pos..e].to_string(), ValueKind::Bare, e)
                }
            }
        } else {
            let e = bare_end(b, pos);
            (s[pos..e].to_string(), ValueKind::Bare, e)
        };
        // Trailing `)` close open groups when the term has no unmatched `(` itself.
        let term_end = end;
        let mut value = value;
        let mut end = end;
        let mut closers = Vec::new();
        if kind == ValueKind::Bare {
            loop {
                let opens = value.bytes().filter(|&c| c == b'(').count();
                let closes = value.bytes().filter(|&c| c == b')').count();
                if depth > 0 && value.ends_with(')') && closes > opens {
                    value.pop();
                    end -= 1;
                    depth -= 1;
                    closers.push(Span::new(end, end + 1));
                } else {
                    break;
                }
            }
        }
        closers.sort();
        pos = term_end.max(start + 1);
        let span = Span::new(start, end);
        let op = if key.is_none() && !negated && kind == ValueKind::Bare {
            match value.as_str() {
                "AND" => Some(Tok::And(span)),
                "OR" => Some(Tok::Or(span)),
                "NOT" => Some(Tok::Not(span)),
                _ => None,
            }
        } else {
            None
        };
        if let Some(op) = op {
            toks.push(op);
        } else if !(value.is_empty() && key.is_none() && kind == ValueKind::Bare) {
            // (A lone `-` is skipped: an empty bare term means nothing.)
            toks.push(Tok::Atom(
                Atom {
                    key,
                    value,
                    kind,
                    span,
                    value_span: Span::new(value_start, end),
                },
                negated,
            ));
        }
        toks.extend(closers.into_iter().map(Tok::RParen));
    }
    toks
}

fn bare_end(b: &[u8], pos: usize) -> usize {
    let mut e = pos;
    while e < b.len() && !b[e].is_ascii_whitespace() {
        e += 1;
    }
    e
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    len: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn parse_or(&mut self, errors: &mut Vec<QueryError>) -> Option<Expr<(Atom, bool)>> {
        let mut parts = Vec::new();
        let mut first = true;
        loop {
            match self.parse_and(errors) {
                Some(e) => parts.push(e),
                None => {
                    if !first {
                        let span = self
                            .toks
                            .get(self.pos.saturating_sub(1))
                            .map_or(Span::new(self.len, self.len), Tok::span);
                        errors.push(QueryError::new(
                            "dangling_operator",
                            span,
                            "OR needs a term on both sides",
                        ));
                    }
                }
            }
            first = false;
            match self.peek() {
                Some(Tok::Or(_)) => {
                    if parts.is_empty() {
                        errors.push(QueryError::new(
                            "dangling_operator",
                            self.toks[self.pos].span(),
                            "OR needs a term on both sides",
                        ));
                    }
                    self.pos += 1;
                }
                _ => break,
            }
        }
        match parts.len() {
            0 => None,
            1 => parts.pop(),
            _ => Some(Expr::Or(parts)),
        }
    }

    fn parse_and(&mut self, errors: &mut Vec<QueryError>) -> Option<Expr<(Atom, bool)>> {
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                None | Some(Tok::Or(_)) | Some(Tok::RParen(_)) => break,
                Some(Tok::And(s)) => {
                    let s = *s;
                    self.pos += 1;
                    if parts.is_empty()
                        || matches!(self.peek(), None | Some(Tok::Or(_)) | Some(Tok::RParen(_)))
                    {
                        errors.push(QueryError::new(
                            "dangling_operator",
                            s,
                            "AND needs a term on both sides",
                        ));
                    }
                }
                _ => {
                    if let Some(e) = self.parse_unary(errors) {
                        parts.push(e);
                    }
                }
            }
        }
        match parts.len() {
            0 => None,
            1 => parts.pop(),
            _ => Some(Expr::And(parts)),
        }
    }

    fn parse_unary(&mut self, errors: &mut Vec<QueryError>) -> Option<Expr<(Atom, bool)>> {
        let tok = self.toks.get(self.pos)?.clone();
        self.pos += 1;
        match tok {
            Tok::Not(s) => match self.peek() {
                None | Some(Tok::Or(_)) | Some(Tok::RParen(_)) | Some(Tok::And(_)) => {
                    errors.push(QueryError::new(
                        "dangling_operator",
                        s,
                        "NOT needs a term after it",
                    ));
                    None
                }
                _ => self.parse_unary(errors).map(|e| Expr::Not(Box::new(e))),
            },
            Tok::LParen(s) => {
                let inner = self.parse_or(errors);
                match self.peek() {
                    Some(Tok::RParen(_)) => {
                        self.pos += 1;
                    }
                    _ => {
                        errors.push(QueryError::new(
                            "unbalanced_paren",
                            s,
                            "missing closing parenthesis",
                        ));
                    }
                }
                if inner.is_none() {
                    errors.push(QueryError::new("empty_group", s, "empty parentheses"));
                }
                inner
            }
            Tok::Atom(a, negated) => {
                if negated {
                    Some(Expr::Not(Box::new(Expr::Term((a, true)))))
                } else {
                    Some(Expr::Term((a, false)))
                }
            }
            Tok::RParen(_) | Tok::And(_) | Tok::Or(_) => None,
        }
    }
}

/// Lexes and parses a query into a boolean expression over atoms (each with its `-` flag).
/// `None` for an empty query. Errors carry positions; the expression is still returned
/// when it could be built, so callers may report every error at once.
pub fn parse(query: &str, opts: &LexOptions<'_>) -> (Option<Expr<Atom>>, Vec<QueryError>) {
    let mut errors = Vec::new();
    let toks = lex(query, opts, &mut errors);
    let mut p = Parser {
        toks,
        pos: 0,
        len: query.len(),
    };
    let expr = p.parse_or(&mut errors);
    while p.pos < p.toks.len() {
        // Only an unmatched `)` can remain; the lexer emits those only inside a group, so
        // this is defensive.
        let span = p.toks[p.pos].span();
        errors.push(QueryError::new(
            "unbalanced_paren",
            span,
            "unexpected closing parenthesis",
        ));
        p.pos += 1;
    }
    let expr = expr.map(strip_flags);
    (expr, errors)
}

fn strip_flags(e: Expr<(Atom, bool)>) -> Expr<Atom> {
    match e {
        Expr::Term((a, _)) => Expr::Term(a),
        Expr::Not(inner) => Expr::Not(Box::new(strip_flags(*inner))),
        Expr::And(v) => Expr::And(v.into_iter().map(strip_flags).collect()),
        Expr::Or(v) => Expr::Or(v.into_iter().map(strip_flags).collect()),
    }
}

/// Pulls top-level positive terms matching `pick` out of a conjunction (for modifiers such
/// as `sort:` or `case:` that are not boolean conditions). Matching terms anywhere else are
/// reported by `misplaced` as errors. Returns the remaining expression and the picked terms.
pub fn extract_modifiers<T: Clone>(
    expr: Option<Expr<T>>,
    pick: &dyn Fn(&T) -> bool,
    misplaced: &mut dyn FnMut(&T),
) -> (Option<Expr<T>>, Vec<T>) {
    let Some(expr) = expr else {
        return (None, Vec::new());
    };
    let mut picked = Vec::new();
    let rest = strip(expr, true, pick, misplaced, &mut picked);
    (rest, picked)
}

/// Removes modifier terms that are members of a conjunction (or the whole query) and not
/// under a negation, so `a OR b sort:x` applies `sort:` globally. Others are misplaced.
fn strip<T: Clone>(
    e: Expr<T>,
    in_and: bool,
    pick: &dyn Fn(&T) -> bool,
    misplaced: &mut dyn FnMut(&T),
    picked: &mut Vec<T>,
) -> Option<Expr<T>> {
    match e {
        Expr::Term(t) if pick(&t) => {
            if in_and {
                picked.push(t);
                None
            } else {
                misplaced(&t);
                Some(Expr::Term(t))
            }
        }
        Expr::Term(t) => Some(Expr::Term(t)),
        Expr::Not(inner) => {
            for t in inner.terms() {
                if pick(t) {
                    misplaced(t);
                }
            }
            Some(Expr::Not(inner))
        }
        Expr::And(v) => {
            let mut kept: Vec<Expr<T>> = v
                .into_iter()
                .filter_map(|c| strip(c, true, pick, misplaced, picked))
                .collect();
            match kept.len() {
                0 => None,
                1 => kept.pop(),
                _ => Some(Expr::And(kept)),
            }
        }
        Expr::Or(v) => {
            let kept: Vec<Expr<T>> = v
                .into_iter()
                .filter_map(|c| strip(c, false, pick, misplaced, picked))
                .collect();
            Some(Expr::Or(kept))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> LexOptions<'static> {
        LexOptions {
            regex: true,
            is_qualifier: &|k| matches!(k, "path" | "lang" | "repo"),
            strict_keys: false,
        }
    }

    fn show(q: &str) -> String {
        let (e, errs) = parse(q, &opts());
        assert!(errs.is_empty(), "{q}: {errs:?}");
        e.map(|e| {
            e.render(&|a: &Atom| match &a.key {
                Some(k) => format!("{k}:{}", quote_value(&a.value, a.kind)),
                None => quote_value(&a.value, a.kind),
            })
        })
        .unwrap_or_default()
    }

    fn errs(q: &str) -> Vec<&'static str> {
        parse(q, &opts()).1.into_iter().map(|e| e.subcode).collect()
    }

    #[test]
    fn structure() {
        assert_eq!(show("a b"), "a b");
        assert_eq!(show("a AND b"), "a b");
        assert_eq!(show("a OR b c"), "a OR b c");
        assert_eq!(show("(a OR b) c"), "(a OR b) c");
        assert_eq!(show("-a NOT b"), "-a -b");
        assert_eq!(show("NOT (a OR b)"), "NOT (a OR b)");
        assert_eq!(show("((a))"), "a");
        assert_eq!(show(""), "");
    }

    #[test]
    fn values() {
        assert_eq!(show("\"a b\" path:\"x y\""), "\"a b\" path:\"x y\"");
        assert_eq!(show(r#""say \"hi\"""#), r#""say \"hi\"""#);
        assert_eq!(show("/fo+/ path:/^src\\//"), "/fo+/ path:/^src\\//");
        assert_eq!(show("/usr/bin"), "/usr/bin");
        assert_eq!(show("std::io foo:bar"), "std::io foo:bar");
        assert_eq!(show("charge_card( foo()"), "charge_card( \"foo()\"");
        assert_eq!(show("(foo() OR bar)"), "\"foo()\" OR bar");
        assert_eq!(show("lang:rust path:src/"), "lang:rust path:src/");
    }

    #[test]
    fn spans() {
        let (e, _) = parse("ab -path:x", &opts());
        let e = e.unwrap();
        let terms = e.terms();
        assert_eq!(terms[0].span, Span::new(0, 2));
        assert_eq!(terms[1].span, Span::new(3, 10));
        assert_eq!(terms[1].value_span, Span::new(9, 10));
    }

    #[test]
    fn errors() {
        assert_eq!(errs("(a b"), vec!["unbalanced_paren"]);
        assert_eq!(errs("a OR"), vec!["dangling_operator"]);
        assert_eq!(errs("OR a"), vec!["dangling_operator"]);
        assert_eq!(errs("a AND"), vec!["dangling_operator"]);
        assert_eq!(errs("NOT"), vec!["dangling_operator"]);
        assert_eq!(errs("\"abc"), vec!["unterminated_quote"]);
        assert_eq!(errs("()"), vec!["empty_group"]);
        let strict = LexOptions {
            regex: false,
            is_qualifier: &|k| k == "label",
            strict_keys: true,
        };
        let (_, e) = parse("label:x colour:red", &strict);
        assert_eq!(e[0].subcode, "unknown_qualifier");
        assert_eq!(e[0].span, Span::new(8, 14));
    }

    #[test]
    fn modifiers() {
        let (e, _) = parse("a path:x (b OR path:y)", &opts());
        let mut bad = Vec::new();
        let (rest, picked) = extract_modifiers(
            e,
            &|a: &Atom| a.key.as_deref() == Some("path"),
            &mut |a: &Atom| bad.push(a.value.clone()),
        );
        assert_eq!(picked.len(), 1);
        assert_eq!(bad, vec!["y".to_string()]);
        assert_eq!(rest.unwrap().terms().len(), 3);
    }

    #[test]
    fn bee_error_shape() {
        let e = to_bee_error(&[QueryError::new("bad_regex", Span::new(0, 3), "x")]);
        assert_eq!(e.reason, Reason::InvalidArgument);
        assert_eq!(
            e.payload.get("subcode"),
            Some(&PayloadValue::from(BAD_QUERY))
        );
        let e = to_bee_error(&[QueryError::new("regex_too_large", Span::new(0, 3), "x")]);
        assert_eq!(e.reason, Reason::TooLarge);
    }
}
