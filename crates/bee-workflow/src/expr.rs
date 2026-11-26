//! The expression language: `${{ }}` templates, literals, operators, functions and contexts.
//!
//! Semantics follow GitHub's documentation ("Evaluate expressions in workflows and actions"):
//!
//! * Literals: `null`, `true`, `false`, numbers in any JSON form plus hex (`0xff`), octal
//!   (`0o17`), `NaN` and `Infinity`, and single-quoted strings with `''` as the escape.
//! * Operators, tightest first: `( )`, `[ ]`, `.`, `!`, `< <= > >=`, `== !=`, `&&`, `||`.
//!   `&&` and `||` short-circuit and return an operand, not a boolean.
//! * Comparisons are loose: operands of different kinds are coerced to numbers, strings
//!   compare case-insensitively, arrays and objects are equal only to themselves, `NaN` is
//!   unequal to everything.
//! * Property dereference on anything that is not an object gives `null`; `*` filters every
//!   element (`github.event.issue.labels.*.name`).
//! * Context names, property names and function names are case-insensitive.

use std::fmt;

use crate::value::{Object, Value, eq_ignore_case};

/// GitHub's limit on the length of one expression.
pub const MAX_EXPRESSION_LENGTH: usize = 21_000;
/// GitHub's limit on expression nesting depth.
pub const MAX_EXPRESSION_DEPTH: usize = 50;

/// An expression parse or evaluation error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExprError {
    /// What is wrong.
    pub message: String,
    /// Character offset in the expression text, when known.
    pub offset: Option<usize>,
}

impl ExprError {
    fn at(offset: usize, message: impl Into<String>) -> Self {
        ExprError {
            message: message.into(),
            offset: Some(offset),
        }
    }

    fn eval(message: impl Into<String>) -> Self {
        ExprError {
            message: message.into(),
            offset: None,
        }
    }
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(o) => write!(f, "{} (at character {})", self.message, o + 1),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ExprError {}

/// A built-in function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Function {
    /// `contains(search, item)`.
    Contains,
    /// `startsWith(search, prefix)`.
    StartsWith,
    /// `endsWith(search, suffix)`.
    EndsWith,
    /// `format(fmt, args...)`.
    Format,
    /// `join(array, sep?)`.
    Join,
    /// `toJSON(value)`.
    ToJson,
    /// `fromJSON(text)`.
    FromJson,
    /// `hashFiles(patterns...)`.
    HashFiles,
    /// `success()`.
    Success,
    /// `always()`.
    Always,
    /// `cancelled()`.
    Cancelled,
    /// `failure()`.
    Failure,
}

impl Function {
    /// All functions.
    pub const ALL: [Function; 12] = [
        Function::Contains,
        Function::StartsWith,
        Function::EndsWith,
        Function::Format,
        Function::Join,
        Function::ToJson,
        Function::FromJson,
        Function::HashFiles,
        Function::Success,
        Function::Always,
        Function::Cancelled,
        Function::Failure,
    ];

    /// The documented spelling.
    pub fn name(self) -> &'static str {
        match self {
            Function::Contains => "contains",
            Function::StartsWith => "startsWith",
            Function::EndsWith => "endsWith",
            Function::Format => "format",
            Function::Join => "join",
            Function::ToJson => "toJSON",
            Function::FromJson => "fromJSON",
            Function::HashFiles => "hashFiles",
            Function::Success => "success",
            Function::Always => "always",
            Function::Cancelled => "cancelled",
            Function::Failure => "failure",
        }
    }

    /// Looks a function up case-insensitively.
    pub fn lookup(name: &str) -> Option<Function> {
        Function::ALL
            .into_iter()
            .find(|f| f.name().eq_ignore_ascii_case(name))
    }

    /// Minimum and maximum argument counts.
    pub fn arity(self) -> (usize, usize) {
        match self {
            Function::Contains | Function::StartsWith | Function::EndsWith => (2, 2),
            Function::Format => (1, usize::MAX),
            Function::Join => (1, 2),
            Function::ToJson | Function::FromJson => (1, 1),
            Function::HashFiles => (1, usize::MAX),
            Function::Success | Function::Always | Function::Cancelled | Function::Failure => {
                (0, 0)
            }
        }
    }

    /// Whether this is a job-status check function.
    pub fn is_status(self) -> bool {
        matches!(
            self,
            Function::Success | Function::Always | Function::Cancelled | Function::Failure
        )
    }
}

/// A binary comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A parsed expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A literal value.
    Literal(Value),
    /// A context reference (`github`, `matrix`, ...), stored lower-case.
    Context(String),
    /// `left.name`.
    Property(Box<Expr>, String),
    /// `left[index]`.
    Index(Box<Expr>, Box<Expr>),
    /// `left.*` or `left[*]`.
    Wildcard(Box<Expr>),
    /// `!operand`.
    Not(Box<Expr>),
    /// A comparison.
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    /// `left && right`.
    And(Box<Expr>, Box<Expr>),
    /// `left || right`.
    Or(Box<Expr>, Box<Expr>),
    /// A function call.
    Call(Function, Vec<Expr>),
}

impl Expr {
    /// Every context name referenced, lower-case, sorted and deduplicated.
    pub fn contexts(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Expr::Context(n) = e {
                out.push(n.clone());
            }
        });
        out.sort();
        out.dedup();
        out
    }

    /// Every function called, sorted and deduplicated.
    pub fn functions(&self) -> Vec<Function> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Expr::Call(f, _) = e {
                out.push(*f);
            }
        });
        out.sort();
        out.dedup();
        out
    }

    /// Whether a status function (`success`, `always`, `cancelled`, `failure`) is called.
    pub fn has_status_function(&self) -> bool {
        self.functions().iter().any(|f| f.is_status())
    }

    fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Literal(_) | Expr::Context(_) => {}
            Expr::Property(e, _) | Expr::Wildcard(e) | Expr::Not(e) => e.walk(f),
            Expr::Index(a, b) | Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
                a.walk(f);
                b.walk(f);
            }
            Expr::Call(_, args) => {
                for a in args {
                    a.walk(f);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Lexer

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Star,
    Not,
    Cmp(CmpOp),
    And,
    Or,
    Number(f64),
    Str(String),
    Null,
    True,
    False,
    Ident(String),
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn lex(src: &str) -> Result<Vec<(Tok, usize)>, ExprError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        let two = |n: char| chars.get(i + 1) == Some(&n);
        let tok = match c {
            ' ' | '\t' | '\n' | '\r' => {
                i += 1;
                continue;
            }
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            '[' => Tok::LBracket,
            ']' => Tok::RBracket,
            ',' => Tok::Comma,
            '*' => Tok::Star,
            '!' if two('=') => {
                i += 1;
                Tok::Cmp(CmpOp::Ne)
            }
            '!' => Tok::Not,
            '=' if two('=') => {
                i += 1;
                Tok::Cmp(CmpOp::Eq)
            }
            '<' if two('=') => {
                i += 1;
                Tok::Cmp(CmpOp::Le)
            }
            '<' => Tok::Cmp(CmpOp::Lt),
            '>' if two('=') => {
                i += 1;
                Tok::Cmp(CmpOp::Ge)
            }
            '>' => Tok::Cmp(CmpOp::Gt),
            '&' if two('&') => {
                i += 1;
                Tok::And
            }
            '|' if two('|') => {
                i += 1;
                Tok::Or
            }
            '\'' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(ExprError::at(start, "unterminated string literal")),
                        Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                            s.push('\'');
                            i += 2;
                        }
                        Some('\'') => break,
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                Tok::Str(s)
            }
            '.' if !chars.get(i + 1).is_some_and(char::is_ascii_digit)
                || matches!(
                    toks.last(),
                    Some((Tok::Ident(_) | Tok::RParen | Tok::RBracket | Tok::Star, _))
                ) =>
            {
                Tok::Dot
            }
            c if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => {
                let mut j = i;
                while j < chars.len()
                    && (chars[j].is_ascii_alphanumeric()
                        || chars[j] == '.'
                        || chars[j] == '_'
                        || ((chars[j] == '-' || chars[j] == '+')
                            && (j == i || matches!(chars[j - 1], 'e' | 'E'))))
                {
                    j += 1;
                }
                let text: String = chars[i..j].iter().collect();
                let n = parse_number_literal(&text)
                    .ok_or_else(|| ExprError::at(start, format!("unexpected symbol: '{text}'")))?;
                i = j;
                toks.push((Tok::Number(n), start));
                continue;
            }
            c if is_ident_start(c) => {
                let mut j = i;
                while j < chars.len() && is_ident_char(chars[j]) {
                    j += 1;
                }
                let text: String = chars[i..j].iter().collect();
                i = j;
                // Keywords are recognised only where a value can start (not after `.`).
                let after_dot = matches!(toks.last(), Some((Tok::Dot, _)));
                let tok = if after_dot {
                    Tok::Ident(text)
                } else {
                    match text.as_str() {
                        "null" => Tok::Null,
                        "true" => Tok::True,
                        "false" => Tok::False,
                        "NaN" => Tok::Number(f64::NAN),
                        "Infinity" => Tok::Number(f64::INFINITY),
                        _ => Tok::Ident(text),
                    }
                };
                toks.push((tok, start));
                continue;
            }
            other => {
                return Err(ExprError::at(
                    start,
                    format!("unexpected symbol: '{other}'"),
                ));
            }
        };
        toks.push((tok, start));
        i += 1;
    }
    Ok(toks)
}

/// Parses an expression number literal: JSON numbers, `0x` hex, `0o` octal, `-Infinity`.
fn parse_number_literal(text: &str) -> Option<f64> {
    if text == "-Infinity" {
        return Some(f64::NEG_INFINITY);
    }
    let (neg, body) = match text.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let v = if let Some(h) = body.strip_prefix("0x") {
        if h.is_empty() || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        crate::value::parse_number(&format!("0x{h}"))
    } else if let Some(o) = body.strip_prefix("0o") {
        if o.is_empty() || !o.chars().all(|c| ('0'..='7').contains(&c)) {
            return None;
        }
        crate::value::parse_number(&format!("0o{o}"))
    } else {
        if body.is_empty()
            || !body
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        {
            return None;
        }
        let n = crate::value::parse_number(body);
        if n.is_nan() {
            return None;
        }
        n
    };
    Some(if neg { -v } else { v })
}

// ---------------------------------------------------------------------------------------------
// Parser

/// Parses one expression (the text between `${{` and `}}`).
pub fn parse(src: &str) -> Result<Expr, ExprError> {
    if src.chars().count() > MAX_EXPRESSION_LENGTH {
        return Err(ExprError::at(
            0,
            format!("exceeded max expression length {MAX_EXPRESSION_LENGTH}"),
        ));
    }
    let toks = lex(src)?;
    if toks.is_empty() {
        return Err(ExprError::at(0, "an expression was expected"));
    }
    let mut p = ExprParser {
        toks,
        i: 0,
        depth: 0,
        len: src.chars().count(),
    };
    let e = p.or()?;
    if let Some((t, off)) = p.toks.get(p.i) {
        return Err(ExprError::at(
            *off,
            format!("unexpected symbol: '{}'", tok_text(t)),
        ));
    }
    Ok(e)
}

fn tok_text(t: &Tok) -> String {
    match t {
        Tok::LParen => "(".into(),
        Tok::RParen => ")".into(),
        Tok::LBracket => "[".into(),
        Tok::RBracket => "]".into(),
        Tok::Dot => ".".into(),
        Tok::Comma => ",".into(),
        Tok::Star => "*".into(),
        Tok::Not => "!".into(),
        Tok::Cmp(op) => match op {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
        .into(),
        Tok::And => "&&".into(),
        Tok::Or => "||".into(),
        Tok::Number(n) => crate::value::format_number(*n),
        Tok::Str(s) => format!("'{s}'"),
        Tok::Null => "null".into(),
        Tok::True => "true".into(),
        Tok::False => "false".into(),
        Tok::Ident(s) => s.clone(),
    }
}

struct ExprParser {
    toks: Vec<(Tok, usize)>,
    i: usize,
    depth: usize,
    len: usize,
}

impl ExprParser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i).map(|(t, _)| t)
    }

    fn offset(&self) -> usize {
        self.toks.get(self.i).map_or(self.len, |(_, o)| *o)
    }

    fn deeper(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_EXPRESSION_DEPTH {
            return Err(ExprError::at(
                self.offset(),
                format!("exceeded max expression depth {MAX_EXPRESSION_DEPTH}"),
            ));
        }
        Ok(())
    }

    fn expect(&mut self, t: Tok, what: &str) -> Result<(), ExprError> {
        if self.peek() == Some(&t) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.unexpected(what))
        }
    }

    fn unexpected(&self, what: &str) -> ExprError {
        match self.toks.get(self.i) {
            Some((t, off)) => ExprError::at(
                *off,
                format!("unexpected symbol: '{}'; expected {what}", tok_text(t)),
            ),
            None => ExprError::at(
                self.len,
                format!("unexpected end of expression; expected {what}"),
            ),
        }
    }

    fn or(&mut self) -> Result<Expr, ExprError> {
        self.deeper()?;
        let mut left = self.and()?;
        while self.peek() == Some(&Tok::Or) {
            self.i += 1;
            let right = self.and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.equality()?;
        while self.peek() == Some(&Tok::And) {
            self.i += 1;
            let right = self.equality()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn equality(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.relational()?;
        while let Some(Tok::Cmp(op @ (CmpOp::Eq | CmpOp::Ne))) = self.peek().cloned() {
            self.i += 1;
            let right = self.relational()?;
            left = Expr::Cmp(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn relational(&mut self) -> Result<Expr, ExprError> {
        let mut left = self.unary()?;
        while let Some(Tok::Cmp(op @ (CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge))) =
            self.peek().cloned()
        {
            self.i += 1;
            let right = self.unary()?;
            left = Expr::Cmp(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, ExprError> {
        if self.peek() == Some(&Tok::Not) {
            self.i += 1;
            self.deeper()?;
            let e = self.unary()?;
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(e)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, ExprError> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.i += 1;
                    match self.peek().cloned() {
                        Some(Tok::Star) => {
                            self.i += 1;
                            e = Expr::Wildcard(Box::new(e));
                        }
                        Some(Tok::Ident(name)) => {
                            self.i += 1;
                            e = Expr::Property(Box::new(e), name);
                        }
                        _ => return Err(self.unexpected("a property name or '*'")),
                    }
                }
                Some(Tok::LBracket) => {
                    self.i += 1;
                    if self.peek() == Some(&Tok::Star) {
                        self.i += 1;
                        self.expect(Tok::RBracket, "']'")?;
                        e = Expr::Wildcard(Box::new(e));
                    } else {
                        let idx = self.or()?;
                        self.expect(Tok::RBracket, "']'")?;
                        e = Expr::Index(Box::new(e), Box::new(idx));
                    }
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, ExprError> {
        let off = self.offset();
        let Some(t) = self.peek().cloned() else {
            return Err(self.unexpected("a value"));
        };
        self.i += 1;
        match t {
            Tok::Null => Ok(Expr::Literal(Value::Null)),
            Tok::True => Ok(Expr::Literal(Value::Bool(true))),
            Tok::False => Ok(Expr::Literal(Value::Bool(false))),
            Tok::Number(n) => Ok(Expr::Literal(Value::Number(n))),
            Tok::Str(s) => Ok(Expr::Literal(Value::String(s))),
            Tok::LParen => {
                let e = self.or()?;
                self.expect(Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::Ident(name) => {
                if self.peek() == Some(&Tok::LParen) {
                    self.i += 1;
                    let Some(f) = Function::lookup(&name) else {
                        return Err(ExprError::at(
                            off,
                            format!("Unrecognized function: '{name}'"),
                        ));
                    };
                    let mut args = Vec::new();
                    if self.peek() == Some(&Tok::RParen) {
                        self.i += 1;
                    } else {
                        loop {
                            args.push(self.or()?);
                            match self.peek() {
                                Some(Tok::Comma) => self.i += 1,
                                Some(Tok::RParen) => {
                                    self.i += 1;
                                    break;
                                }
                                _ => return Err(self.unexpected("',' or ')'")),
                            }
                        }
                    }
                    let (min, max) = f.arity();
                    if args.len() < min {
                        return Err(ExprError::at(
                            off,
                            format!(
                                "Too few parameters supplied: '{}' needs at least {min}",
                                f.name()
                            ),
                        ));
                    }
                    if args.len() > max {
                        return Err(ExprError::at(
                            off,
                            format!(
                                "Too many parameters supplied: '{}' takes at most {max}",
                                f.name()
                            ),
                        ));
                    }
                    Ok(Expr::Call(f, args))
                } else {
                    Ok(Expr::Context(name.to_ascii_lowercase()))
                }
            }
            other => {
                self.i -= 1;
                let _ = other;
                Err(self.unexpected("a value"))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Evaluation

/// The results the status functions report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// `success()`.
    pub success: bool,
    /// `failure()`.
    pub failure: bool,
    /// `cancelled()`.
    pub cancelled: bool,
}

impl Status {
    /// Nothing has failed and nothing was cancelled.
    pub const SUCCESS: Status = Status {
        success: true,
        failure: false,
        cancelled: false,
    };
}

impl Default for Status {
    fn default() -> Self {
        Status::SUCCESS
    }
}

/// Computes `hashFiles` for the files of the workspace.
pub trait FileHasher {
    /// Returns the hash of the files matching `patterns` (GitHub returns `""` when none
    /// match), or an error message.
    fn hash_files(&self, patterns: &[String]) -> Result<String, String>;
}

/// Supplies contexts and runtime facts to the evaluator.
pub trait ContextProvider {
    /// The value of a context, by lower-case name (`github`, `matrix`, ...).
    fn context(&self, name: &str) -> Option<Value>;

    /// What the status functions return.
    fn status(&self) -> Status {
        Status::SUCCESS
    }

    /// The file hasher for `hashFiles`, when one is available here.
    fn file_hasher(&self) -> Option<&dyn FileHasher> {
        None
    }
}

/// A simple context provider over a map of named contexts.
#[derive(Default)]
pub struct MapContext<'a> {
    /// The contexts by lower-case name.
    pub contexts: Object,
    /// Status function results.
    pub status: Status,
    /// The file hasher, if any.
    pub hasher: Option<&'a dyn FileHasher>,
}

impl MapContext<'_> {
    /// Sets a context.
    pub fn with(mut self, name: &str, value: Value) -> Self {
        self.contexts.insert(name.to_ascii_lowercase(), value);
        self
    }
}

impl ContextProvider for MapContext<'_> {
    fn context(&self, name: &str) -> Option<Value> {
        self.contexts.get(name).cloned()
    }

    fn status(&self) -> Status {
        self.status
    }

    fn file_hasher(&self) -> Option<&dyn FileHasher> {
        self.hasher
    }
}

/// An intermediate result: a plain value, or the output of a `*` filter whose following
/// dereferences apply to each element.
enum Ev {
    Val(Value),
    Filtered(Vec<Value>),
}

impl Ev {
    fn into_value(self) -> Value {
        match self {
            Ev::Val(v) => v,
            Ev::Filtered(items) => Value::array(items),
        }
    }
}

/// Evaluates a parsed expression.
pub fn evaluate(expr: &Expr, ctx: &dyn ContextProvider) -> Result<Value, ExprError> {
    eval(expr, ctx).map(Ev::into_value)
}

fn deref(v: &Value, key: &Value) -> Option<Value> {
    match v {
        Value::Object(o) => match key {
            Value::String(k) => o.get(k).cloned(),
            Value::Array(_) | Value::Object(_) => None,
            other => o.get(&other.to_display_string()).cloned(),
        },
        Value::Array(a) => {
            let n = match key {
                Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {
                    key.to_number()
                }
                _ => return None,
            };
            if matches!(key, Value::String(_)) && n.is_nan() {
                return None;
            }
            if !n.is_finite() || n < 0.0 {
                return None;
            }
            a.get(n.floor() as usize).cloned()
        }
        _ => None,
    }
}

fn eval(expr: &Expr, ctx: &dyn ContextProvider) -> Result<Ev, ExprError> {
    Ok(match expr {
        Expr::Literal(v) => Ev::Val(v.clone()),
        Expr::Context(name) => Ev::Val(ctx.context(name).unwrap_or(Value::Null)),
        Expr::Property(base, name) => {
            let key = Value::String(name.clone());
            index_ev(eval(base, ctx)?, &key)
        }
        Expr::Index(base, idx) => {
            let b = eval(base, ctx)?;
            let key = evaluate(idx, ctx)?;
            index_ev(b, &key)
        }
        Expr::Wildcard(base) => match eval(base, ctx)? {
            Ev::Val(Value::Array(a)) => Ev::Filtered(a.as_ref().clone()),
            Ev::Val(Value::Object(o)) => Ev::Filtered(o.iter().map(|(_, v)| v.clone()).collect()),
            Ev::Val(_) => Ev::Filtered(Vec::new()),
            Ev::Filtered(items) => {
                let mut out = Vec::new();
                for item in items {
                    match item {
                        Value::Array(a) => out.extend(a.iter().cloned()),
                        Value::Object(o) => out.extend(o.iter().map(|(_, v)| v.clone())),
                        _ => {}
                    }
                }
                Ev::Filtered(out)
            }
        },
        Expr::Not(e) => Ev::Val(Value::Bool(!evaluate(e, ctx)?.is_truthy())),
        Expr::Cmp(op, a, b) => {
            let (l, r) = (evaluate(a, ctx)?, evaluate(b, ctx)?);
            let res = match op {
                CmpOp::Eq => l.loose_eq(&r),
                CmpOp::Ne => !l.loose_eq(&r),
                CmpOp::Lt => l.loose_cmp(&r) == Some(std::cmp::Ordering::Less),
                CmpOp::Le => matches!(
                    l.loose_cmp(&r),
                    Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                ),
                CmpOp::Gt => l.loose_cmp(&r) == Some(std::cmp::Ordering::Greater),
                CmpOp::Ge => matches!(
                    l.loose_cmp(&r),
                    Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                ),
            };
            Ev::Val(Value::Bool(res))
        }
        Expr::And(a, b) => {
            let l = evaluate(a, ctx)?;
            if !l.is_truthy() {
                Ev::Val(l)
            } else {
                Ev::Val(evaluate(b, ctx)?)
            }
        }
        Expr::Or(a, b) => {
            let l = evaluate(a, ctx)?;
            if l.is_truthy() {
                Ev::Val(l)
            } else {
                Ev::Val(evaluate(b, ctx)?)
            }
        }
        Expr::Call(f, args) => Ev::Val(call(*f, args, ctx)?),
    })
}

fn index_ev(base: Ev, key: &Value) -> Ev {
    match base {
        Ev::Val(v) => Ev::Val(deref(&v, key).unwrap_or(Value::Null)),
        Ev::Filtered(items) => Ev::Filtered(items.iter().filter_map(|i| deref(i, key)).collect()),
    }
}

fn call(f: Function, args: &[Expr], ctx: &dyn ContextProvider) -> Result<Value, ExprError> {
    let arg = |i: usize| -> Result<Value, ExprError> { evaluate(&args[i], ctx) };
    Ok(match f {
        Function::Contains => {
            let search = arg(0)?;
            let item = arg(1)?;
            let found = match &search {
                Value::Array(a) => a.iter().any(|v| v.loose_eq(&item)),
                _ => {
                    let hay = search.to_display_string().to_lowercase();
                    hay.contains(&item.to_display_string().to_lowercase())
                }
            };
            Value::Bool(found)
        }
        Function::StartsWith => {
            let (s, p) = (arg(0)?.to_display_string(), arg(1)?.to_display_string());
            Value::Bool(s.to_lowercase().starts_with(&p.to_lowercase()))
        }
        Function::EndsWith => {
            let (s, p) = (arg(0)?.to_display_string(), arg(1)?.to_display_string());
            Value::Bool(s.to_lowercase().ends_with(&p.to_lowercase()))
        }
        Function::Format => {
            let fmt = arg(0)?.to_display_string();
            let mut values = Vec::with_capacity(args.len() - 1);
            for i in 1..args.len() {
                values.push(arg(i)?.to_display_string());
            }
            Value::String(format_string(&fmt, &values)?)
        }
        Function::Join => {
            let v = arg(0)?;
            let sep = if args.len() > 1 {
                arg(1)?.to_display_string()
            } else {
                ",".to_owned()
            };
            match v {
                Value::Array(a) => Value::String(
                    a.iter()
                        .map(Value::to_display_string)
                        .collect::<Vec<_>>()
                        .join(&sep),
                ),
                other => Value::String(other.to_display_string()),
            }
        }
        Function::ToJson => Value::String(arg(0)?.to_json_pretty()),
        Function::FromJson => {
            let text = arg(0)?.to_display_string();
            if text.trim().is_empty() {
                return Err(ExprError::eval(
                    "Error reading JToken from JsonReader. Path '', line 0, position 0.",
                ));
            }
            crate::value::parse_json(&text)
                .map_err(|e| ExprError::eval(format!("fromJSON: {e}")))?
        }
        Function::HashFiles => {
            let mut patterns = Vec::new();
            for i in 0..args.len() {
                patterns.push(arg(i)?.to_display_string());
            }
            let Some(h) = ctx.file_hasher() else {
                return Err(ExprError::eval(
                    "hashFiles is not available in this context",
                ));
            };
            Value::String(h.hash_files(&patterns).map_err(ExprError::eval)?)
        }
        Function::Success => Value::Bool(ctx.status().success),
        Function::Always => Value::Bool(true),
        Function::Cancelled => Value::Bool(ctx.status().cancelled),
        Function::Failure => Value::Bool(ctx.status().failure),
    })
}

/// Implements `format`: `{N}` placeholders, `{{` and `}}` escapes.
pub fn format_string(fmt: &str, args: &[String]) -> Result<String, ExprError> {
    let invalid = || ExprError::eval(format!("The following format string is invalid: '{fmt}'"));
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '{' => {
                let mut j = i + 1;
                let mut n: usize = 0;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    n = n
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(chars[j] as usize - '0' as usize))
                        .ok_or_else(invalid)?;
                    j += 1;
                }
                if j == i + 1 || chars.get(j) != Some(&'}') {
                    return Err(invalid());
                }
                let Some(a) = args.get(n) else {
                    return Err(ExprError::eval(format!(
                        "The following format string references more arguments than were supplied: '{fmt}'"
                    )));
                };
                out.push_str(a);
                i = j + 1;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '}' => return Err(invalid()),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Templates

/// One piece of a template string.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    /// Literal text.
    Text(String),
    /// An embedded `${{ expression }}`.
    Expr {
        /// The expression source, trimmed.
        source: String,
        /// The parsed expression.
        expr: Expr,
    },
}

/// A string that may embed `${{ }}` expressions.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Template {
    /// The pieces in order.
    pub parts: Vec<Part>,
}

impl Template {
    /// Parses a string, finding `${{ ... }}` blocks. The closing `}}` is the first one outside
    /// a string literal.
    pub fn parse(src: &str) -> Result<Template, ExprError> {
        let chars: Vec<char> = src.chars().collect();
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '$' && chars.get(i + 1) == Some(&'{') && chars.get(i + 2) == Some(&'{') {
                let start = i;
                let mut j = i + 3;
                let mut in_str = false;
                let mut end = None;
                while j < chars.len() {
                    let c = chars[j];
                    if in_str {
                        if c == '\'' {
                            if chars.get(j + 1) == Some(&'\'') {
                                j += 2;
                                continue;
                            }
                            in_str = false;
                        }
                    } else if c == '\'' {
                        in_str = true;
                    } else if c == '}' && chars.get(j + 1) == Some(&'}') {
                        end = Some(j);
                        break;
                    }
                    j += 1;
                }
                let Some(end) = end else {
                    return Err(ExprError::at(
                        start,
                        "the expression is not closed: an unescaped '${{' sequence was found, but the closing '}}' sequence was not found",
                    ));
                };
                let inner: String = chars[i + 3..end].iter().collect();
                let source = inner.trim().to_owned();
                if source.is_empty() {
                    return Err(ExprError::at(start, "an expression was expected"));
                }
                let expr = parse(&source).map_err(|e| ExprError {
                    message: e.message,
                    offset: e.offset.map(|o| {
                        o + start + 3 + inner.chars().take_while(|c| c.is_whitespace()).count()
                    }),
                })?;
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                parts.push(Part::Expr { source, expr });
                i = end + 2;
            } else {
                text.push(chars[i]);
                i += 1;
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Template { parts })
    }

    /// A template of plain text.
    pub fn literal(text: &str) -> Template {
        if text.is_empty() {
            Template::default()
        } else {
            Template {
                parts: vec![Part::Text(text.to_owned())],
            }
        }
    }

    /// Whether the template has no expressions.
    pub fn is_literal(&self) -> bool {
        self.parts.iter().all(|p| matches!(p, Part::Text(_)))
    }

    /// The parsed expressions, in order.
    pub fn exprs(&self) -> impl Iterator<Item = &Expr> {
        self.parts.iter().filter_map(|p| match p {
            Part::Expr { expr, .. } => Some(expr),
            Part::Text(_) => None,
        })
    }

    /// Evaluates: a lone expression keeps its value's type; anything else is a string.
    pub fn evaluate(&self, ctx: &dyn ContextProvider) -> Result<Value, ExprError> {
        if let [Part::Expr { expr, .. }] = self.parts.as_slice() {
            return evaluate(expr, ctx);
        }
        Ok(Value::String(self.evaluate_string(ctx)?))
    }

    /// Evaluates to a string, converting each expression result.
    pub fn evaluate_string(&self, ctx: &dyn ContextProvider) -> Result<String, ExprError> {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Text(t) => out.push_str(t),
                Part::Expr { expr, .. } => out.push_str(&evaluate(expr, ctx)?.to_display_string()),
            }
        }
        Ok(out)
    }

    /// Renders the template back to text (`${{ source }}` for expressions).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Text(t) => out.push_str(t),
                Part::Expr { source, .. } => {
                    out.push_str("${{ ");
                    out.push_str(source);
                    out.push_str(" }}");
                }
            }
        }
        out
    }

    /// Substitutes every expression whose contexts are all in `known` (and that calls no
    /// runtime-only function) with its value; leaves the others for runtime.
    pub fn partially_evaluate(
        &self,
        ctx: &dyn ContextProvider,
        known: &[&str],
    ) -> Result<Template, ExprError> {
        let lone = self.parts.len() == 1;
        let mut parts: Vec<Part> = Vec::new();
        for p in &self.parts {
            match p {
                Part::Expr { expr, .. } if is_static(expr, known) => {
                    let v = evaluate(expr, ctx)?;
                    let text = if lone {
                        // A lone expression keeps collections as JSON so nothing is lost.
                        match &v {
                            Value::Array(_) | Value::Object(_) => v.to_json_compact(),
                            other => other.to_display_string(),
                        }
                    } else {
                        v.to_display_string()
                    };
                    push_text(&mut parts, &text);
                }
                Part::Text(t) => push_text(&mut parts, t),
                other => parts.push(other.clone()),
            }
        }
        Ok(Template { parts })
    }
}

fn push_text(parts: &mut Vec<Part>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Part::Text(prev)) = parts.last_mut() {
        prev.push_str(text);
    } else {
        parts.push(Part::Text(text.to_owned()));
    }
}

/// Whether an expression can be evaluated with only the `known` contexts.
pub fn is_static(expr: &Expr, known: &[&str]) -> bool {
    expr.contexts().iter().all(|c| known.contains(&c.as_str()))
        && expr
            .functions()
            .iter()
            .all(|f| !f.is_status() && *f != Function::HashFiles)
}

// ---------------------------------------------------------------------------------------------
// Conditions

/// A parsed `if:` condition, with GitHub's implicit `success() &&` applied.
#[derive(Clone, Debug, PartialEq)]
pub struct Condition {
    /// The source text as written.
    pub source: String,
    /// The expression to evaluate; already wrapped in `success() && (...)` when the source
    /// calls no status function.
    pub expr: CondExpr,
}

/// The two shapes a condition can take.
#[derive(Clone, Debug, PartialEq)]
pub enum CondExpr {
    /// A bare expression, or a single `${{ }}` spanning the whole value.
    Expr(Expr),
    /// Text mixed with `${{ }}` blocks: the rendered string is truthy when non-empty.
    /// The flag says whether the implicit `success()` applies.
    Template(Template, bool),
}

impl Condition {
    /// Parses an `if:` value. An empty value means `success()`.
    pub fn parse(src: &str) -> Result<Condition, ExprError> {
        let trimmed = src.trim();
        let success = || Expr::Call(Function::Success, Vec::new());
        let wrap = |e: Expr| {
            if e.has_status_function() {
                e
            } else {
                Expr::And(Box::new(success()), Box::new(e))
            }
        };
        let expr = if trimmed.is_empty() {
            CondExpr::Expr(success())
        } else if trimmed.contains("${{") {
            let t = Template::parse(trimmed)?;
            match t.parts.as_slice() {
                [Part::Expr { expr, .. }] => CondExpr::Expr(wrap(expr.clone())),
                _ => {
                    let has_status = t.exprs().any(Expr::has_status_function);
                    CondExpr::Template(t, !has_status)
                }
            }
        } else {
            CondExpr::Expr(wrap(parse(trimmed)?))
        };
        Ok(Condition {
            source: src.to_owned(),
            expr,
        })
    }

    /// The default condition, `success()`.
    pub fn default_success() -> Condition {
        Condition {
            source: String::new(),
            expr: CondExpr::Expr(Expr::Call(Function::Success, Vec::new())),
        }
    }

    /// Every expression in the condition.
    pub fn exprs(&self) -> Vec<&Expr> {
        match &self.expr {
            CondExpr::Expr(e) => vec![e],
            CondExpr::Template(t, _) => t.exprs().collect(),
        }
    }

    /// Evaluates the condition to a boolean.
    pub fn evaluate(&self, ctx: &dyn ContextProvider) -> Result<bool, ExprError> {
        match &self.expr {
            CondExpr::Expr(e) => Ok(evaluate(e, ctx)?.is_truthy()),
            CondExpr::Template(t, implicit_success) => {
                if *implicit_success && !ctx.status().success {
                    return Ok(false);
                }
                Ok(!t.evaluate_string(ctx)?.is_empty())
            }
        }
    }
}

/// Parses and evaluates one expression against contexts (a convenience for callers and tests).
pub fn eval_str(src: &str, ctx: &dyn ContextProvider) -> Result<Value, ExprError> {
    evaluate(&parse(src)?, ctx)
}

/// Checks that the string, compared case-insensitively, names one of `names`.
pub fn is_one_of(name: &str, names: &[&str]) -> bool {
    names.iter().any(|n| eq_ignore_case(n, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::parse_json;

    fn ctx() -> MapContext<'static> {
        MapContext::default()
            .with(
                "github",
                parse_json(
                    r#"{"event_name":"push","ref":"refs/heads/main","event":{"issue":{"labels":[{"name":"bug"},{"name":"help wanted"},{"other":1}]},"commits":[{"message":"a","files":["x","y"]},{"message":"b","files":["z"]}]}}"#,
                )
                .unwrap(),
            )
            .with("matrix", parse_json(r#"{"os":"ubuntu-latest","node":18}"#).unwrap())
            .with("env", parse_json(r#"{"NAME":"World","EMPTY":""}"#).unwrap())
    }

    fn ev(src: &str) -> Value {
        eval_str(src, &ctx()).unwrap_or_else(|e| panic!("{src}: {e}"))
    }

    #[test]
    fn literals() {
        // Documented literal examples ("Literals" in GitHub's expression docs).
        assert_eq!(ev("null"), Value::Null);
        assert_eq!(ev("false"), Value::Bool(false));
        assert_eq!(ev("711"), Value::Number(711.0));
        assert_eq!(ev("-9.2"), Value::Number(-9.2));
        assert_eq!(ev("0xff"), Value::Number(255.0));
        assert_eq!(ev("-2.99e-2"), Value::Number(-0.0299));
        assert_eq!(ev("'Mona the Octocat'"), Value::from("Mona the Octocat"));
        assert_eq!(ev("'It''s open source!'"), Value::from("It's open source!"));
        assert!(ev("NaN").to_number().is_nan());
        assert_eq!(ev("Infinity"), Value::Number(f64::INFINITY));
        assert_eq!(ev("0o10"), Value::Number(8.0));
        assert_eq!(ev(".5"), Value::Number(0.5));
    }

    #[test]
    fn operators_and_precedence() {
        assert_eq!(ev("1 < 2 && 2 < 3"), Value::Bool(true));
        assert_eq!(ev("!true || true"), Value::Bool(true));
        assert_eq!(ev("!(true || true)"), Value::Bool(false));
        assert_eq!(ev("1 == 1 == true"), Value::Bool(true));
        assert_eq!(ev("'a' || 'b'"), Value::from("a"));
        assert_eq!(ev("'' || 'b'"), Value::from("b"));
        assert_eq!(ev("0 && 'b'"), Value::Number(0.0));
        assert_eq!(ev("null && 'b'"), Value::Null);
        assert_eq!(ev("1 <= 1"), Value::Bool(true));
        assert_eq!(ev("2 >= 3"), Value::Bool(false));
        assert_eq!(ev("2 > 1"), Value::Bool(true));
        assert_eq!(ev("1 != 2"), Value::Bool(true));
    }

    #[test]
    fn loose_comparisons_documented_examples() {
        // From the "Operators" coercion table: null -> 0, true -> 1, '' -> 0, invalid -> NaN.
        assert_eq!(ev("null == 0"), Value::Bool(true));
        assert_eq!(ev("true == 1"), Value::Bool(true));
        assert_eq!(ev("'' == 0"), Value::Bool(true));
        assert_eq!(ev("'abc' == 0"), Value::Bool(false));
        assert_eq!(ev("'1' == 1"), Value::Bool(true));
        assert_eq!(ev("NaN == NaN"), Value::Bool(false));
        assert_eq!(ev("NaN != NaN"), Value::Bool(true));
        assert_eq!(ev("'abc' < 1"), Value::Bool(false));
        assert_eq!(ev("'abc' >= 1"), Value::Bool(false));
        // Strings compare ignoring case.
        assert_eq!(ev("'ABC' == 'abc'"), Value::Bool(true));
        assert_eq!(ev("'a' < 'B'"), Value::Bool(true));
        assert_eq!(ev("github.event_name == 'PUSH'"), Value::Bool(true));
        // Objects and arrays are only equal to the same instance.
        assert_eq!(ev("github.event == github.event"), Value::Bool(true));
        assert_eq!(ev("fromJSON('[]') == fromJSON('[]')"), Value::Bool(false));
        assert_eq!(ev("github.event == 'Object'"), Value::Bool(false));
    }

    #[test]
    fn property_and_index_dereference() {
        assert_eq!(ev("github.ref"), Value::from("refs/heads/main"));
        assert_eq!(ev("GITHUB.REF"), Value::from("refs/heads/main"));
        assert_eq!(ev("github['ref']"), Value::from("refs/heads/main"));
        assert_eq!(ev("github.missing.deeper"), Value::Null);
        assert_eq!(ev("github.event.commits[1].message"), Value::from("b"));
        assert_eq!(ev("github.event.commits[5]"), Value::Null);
        assert_eq!(ev("github.event.commits['0'].message"), Value::from("a"));
        assert_eq!(ev("github.event.commits[1.7].message"), Value::from("b"));
        assert_eq!(ev("matrix.node"), Value::Number(18.0));
        assert_eq!(ev("nosuchcontext.x"), Value::Null);
    }

    #[test]
    fn object_filters() {
        // Documented: github.event.issue.labels.*.name
        assert_eq!(
            ev("github.event.issue.labels.*.name").to_json_compact(),
            r#"["bug","help wanted"]"#
        );
        assert_eq!(
            ev("toJSON(github.event.commits.*.files.*)"),
            ev("toJSON(fromJSON('[\"x\",\"y\",\"z\"]'))")
        );
        assert_eq!(
            ev("contains(github.event.issue.labels.*.name, 'BUG')"),
            Value::Bool(true)
        );
        assert_eq!(ev("matrix.*").to_json_compact(), r#"["ubuntu-latest",18]"#);
        assert_eq!(ev("github.ref.*").to_json_compact(), "[]");
        assert_eq!(
            ev("github.event.commits[*].message").to_json_compact(),
            r#"["a","b"]"#
        );
    }

    #[test]
    fn contains_starts_ends() {
        // Documented examples.
        assert_eq!(ev("contains('Hello world', 'llo')"), Value::Bool(true));
        assert_eq!(ev("contains('Hello world', 'LLO')"), Value::Bool(true));
        assert_eq!(
            ev("contains(fromJSON('[\"push\", \"pull_request\"]'), github.event_name)"),
            Value::Bool(true)
        );
        assert_eq!(ev("startsWith('Hello world', 'He')"), Value::Bool(true));
        assert_eq!(ev("endsWith('Hello world', 'ld')"), Value::Bool(true));
        assert_eq!(
            ev("startsWith(github.ref, 'refs/tags/')"),
            Value::Bool(false)
        );
        assert_eq!(ev("contains(123, 2)"), Value::Bool(true));
        assert_eq!(ev("contains(fromJSON('[1,2]'), '2')"), Value::Bool(true));
    }

    #[test]
    fn format_function() {
        // Documented: format('Hello {0} {1} {2}', 'Mona', 'the', 'Octocat')
        assert_eq!(
            ev("format('Hello {0} {1} {2}', 'Mona', 'the', 'Octocat')"),
            Value::from("Hello Mona the Octocat")
        );
        // Documented: format('{{Hello {0} {1} {2}!}}', 'Mona', 'the', 'Octocat')
        assert_eq!(
            ev("format('{{Hello {0} {1} {2}!}}', 'Mona', 'the', 'Octocat')"),
            Value::from("{Hello Mona the Octocat!}")
        );
        assert_eq!(ev("format('{0}-{0}', matrix.node)"), Value::from("18-18"));
        assert!(eval_str("format('{1}', 'a')", &ctx()).is_err());
        assert!(eval_str("format('{a}', 'a')", &ctx()).is_err());
        assert!(eval_str("format('}', 'a')", &ctx()).is_err());
        assert!(eval_str("format('{0', 'a')", &ctx()).is_err());
    }

    #[test]
    fn join_and_json() {
        assert_eq!(
            ev("join(github.event.issue.labels.*.name, ', ')"),
            Value::from("bug, help wanted")
        );
        assert_eq!(
            ev("join(fromJSON('[1,true,null]'))"),
            Value::from("1,true,")
        );
        assert_eq!(ev("join('abc')"), Value::from("abc"));
        assert_eq!(
            ev("toJSON(matrix)"),
            Value::from("{\n  \"os\": \"ubuntu-latest\",\n  \"node\": 18\n}")
        );
        assert_eq!(ev("toJSON('x')"), Value::from("\"x\""));
        assert_eq!(ev("toJSON(null)"), Value::from("null"));
        assert_eq!(ev("fromJSON('true')"), Value::Bool(true));
        assert_eq!(
            ev("fromJSON('{\"a\": {\"b\": 3}}').a.b"),
            Value::Number(3.0)
        );
        assert!(eval_str("fromJSON('{')", &ctx()).is_err());
        assert!(eval_str("fromJSON('')", &ctx()).is_err());
    }

    #[test]
    fn status_functions() {
        let mut c = ctx();
        assert_eq!(eval_str("success()", &c).unwrap(), Value::Bool(true));
        assert_eq!(eval_str("always()", &c).unwrap(), Value::Bool(true));
        c.status = Status {
            success: false,
            failure: true,
            cancelled: false,
        };
        assert_eq!(eval_str("failure()", &c).unwrap(), Value::Bool(true));
        assert_eq!(eval_str("SUCCESS()", &c).unwrap(), Value::Bool(false));
        assert_eq!(eval_str("cancelled()", &c).unwrap(), Value::Bool(false));
    }

    struct FixedHasher;
    impl FileHasher for FixedHasher {
        fn hash_files(&self, patterns: &[String]) -> Result<String, String> {
            Ok(format!("hash:{}", patterns.join("|")))
        }
    }

    #[test]
    fn hash_files_uses_the_pluggable_hasher() {
        let hasher = FixedHasher;
        let mut c = ctx();
        assert!(eval_str("hashFiles('**/package-lock.json')", &c).is_err());
        c.hasher = Some(&hasher);
        assert_eq!(
            eval_str("hashFiles('**/package-lock.json', '!x')", &c).unwrap(),
            Value::from("hash:**/package-lock.json|!x")
        );
    }

    #[test]
    fn parse_errors() {
        for bad in [
            "",
            "1 +",
            "a.",
            "(1",
            "foo(1)",
            "contains(1)",
            "always(1)",
            "'open",
            "a b",
            "1 = 2",
            "a[1",
            "!",
            "a.1",
        ] {
            assert!(parse(bad).is_err(), "{bad} should not parse");
        }
        let e = parse("github.ref == 'x' &").unwrap_err();
        assert_eq!(e.offset, Some(18));
        let deep = "(".repeat(60) + "1" + &")".repeat(60);
        assert!(parse(&deep).unwrap_err().message.contains("depth"));
    }

    #[test]
    fn identifiers_may_contain_dashes() {
        let c = MapContext::default().with(
            "steps",
            parse_json(r#"{"my-step":{"outputs":{"v":"1"}}}"#).unwrap(),
        );
        assert_eq!(
            eval_str("steps.my-step.outputs.v", &c).unwrap(),
            Value::from("1")
        );
        assert_eq!(
            parse("steps.x.outputs.null").unwrap().contexts(),
            vec!["steps".to_owned()]
        );
    }

    #[test]
    fn templates() {
        let t = Template::parse("Hello ${{ env.NAME }}! ${{ matrix.node }}").unwrap();
        assert_eq!(t.evaluate(&ctx()).unwrap(), Value::from("Hello World! 18"));
        let lone = Template::parse("${{ matrix.node }}").unwrap();
        assert_eq!(lone.evaluate(&ctx()).unwrap(), Value::Number(18.0));
        let s = Template::parse("${{ '}}' }}x").unwrap();
        assert_eq!(s.evaluate(&ctx()).unwrap(), Value::from("}}x"));
        assert!(Template::parse("${{ env.NAME ").is_err());
        assert!(Template::parse("${{ }}").is_err());
        assert_eq!(Template::parse("no exprs").unwrap().render(), "no exprs");
        let e = Template::parse("ab ${{ 1 + }}").unwrap_err();
        assert!(e.offset.is_some());
    }

    #[test]
    fn partial_evaluation_keeps_runtime_expressions() {
        let t = Template::parse(
            "node ${{ matrix.node }} at ${{ steps.x.outputs.y }} ${{ secrets.TOKEN }}",
        )
        .unwrap();
        let p = t.partially_evaluate(&ctx(), &["matrix", "github"]).unwrap();
        assert_eq!(
            p.render(),
            "node 18 at ${{ steps.x.outputs.y }} ${{ secrets.TOKEN }}"
        );
        // Substituted text is data, never re-parsed as an expression.
        let c = MapContext::default().with(
            "github",
            parse_json(r#"{"title":"${{ secrets.X }}"}"#).unwrap(),
        );
        let p = Template::parse("t=${{ github.title }}")
            .unwrap()
            .partially_evaluate(&c, &["github"])
            .unwrap();
        assert!(p.is_literal());
        assert_eq!(p.parts, vec![Part::Text("t=${{ secrets.X }}".to_owned())]);
    }

    #[test]
    fn conditions_get_implicit_success() {
        let c = Condition::parse("github.event_name == 'push'").unwrap();
        let mut ctx = ctx();
        assert!(c.evaluate(&ctx).unwrap());
        ctx.status = Status {
            success: false,
            failure: true,
            cancelled: false,
        };
        assert!(!c.evaluate(&ctx).unwrap());
        assert!(
            Condition::parse("${{ failure() }}")
                .unwrap()
                .evaluate(&ctx)
                .unwrap()
        );
        assert!(
            Condition::parse("always() && github.event_name == 'push'")
                .unwrap()
                .evaluate(&ctx)
                .unwrap()
        );
        assert!(!Condition::parse("").unwrap().evaluate(&ctx).unwrap());
    }

    #[test]
    fn mixed_text_condition_is_a_non_empty_string() {
        // Documented pitfall: text outside ${{ }} makes the condition a string, which is truthy.
        let c = Condition::parse("${{ false }} && true").unwrap();
        assert!(c.evaluate(&ctx()).unwrap());
        let c = Condition::parse("  ${{ github.event_name == 'pull_request' }}  ").unwrap();
        assert!(!c.evaluate(&ctx()).unwrap());
    }

    #[test]
    fn contexts_and_functions_are_reported() {
        let e = parse("contains(github.ref, 'x') && success() || matrix.os").unwrap();
        assert_eq!(e.contexts(), vec!["github".to_owned(), "matrix".to_owned()]);
        assert!(e.has_status_function());
        assert_eq!(e.functions(), vec![Function::Contains, Function::Success]);
    }
}
