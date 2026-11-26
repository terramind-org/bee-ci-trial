//! A GraphQL lexer and parsers for executable documents (queries, mutations, fragments,
//! variables, directives) and for the schema definition language the schema is written in.
//! Written from the GraphQL specification (October 2021), sections 2 and 3.

use std::collections::BTreeMap;

/// A parse error with its byte offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub message: String,
    pub offset: usize,
}

type PResult<T> = Result<T, ParseError>;

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Punct(char),
    Spread,
    Name(String),
    Int(i64),
    Float(f64),
    Str(String),
    Eof,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn skip_ignored(&mut self) {
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' | b',' => self.pos += 1,
                0xEF if self.src[self.pos..].starts_with(&[0xEF, 0xBB, 0xBF]) => self.pos += 3,
                b'#' => {
                    while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn err(&self, m: impl Into<String>) -> ParseError {
        ParseError {
            message: m.into(),
            offset: self.pos,
        }
    }

    fn next(&mut self) -> PResult<(usize, Tok)> {
        self.skip_ignored();
        let start = self.pos;
        let Some(&c) = self.src.get(self.pos) else {
            return Ok((start, Tok::Eof));
        };
        match c {
            b'!' | b'$' | b'&' | b'(' | b')' | b':' | b'=' | b'@' | b'[' | b']' | b'{' | b'|'
            | b'}' => {
                self.pos += 1;
                Ok((start, Tok::Punct(c as char)))
            }
            b'.' => {
                if self.src[self.pos..].starts_with(b"...") {
                    self.pos += 3;
                    Ok((start, Tok::Spread))
                } else {
                    Err(self.err("unexpected `.`"))
                }
            }
            b'"' => self.string().map(|s| (start, Tok::Str(s))),
            b'-' | b'0'..=b'9' => self.number().map(|t| (start, t)),
            c if c == b'_' || c.is_ascii_alphabetic() => {
                while self.pos < self.src.len()
                    && (self.src[self.pos] == b'_' || self.src[self.pos].is_ascii_alphanumeric())
                {
                    self.pos += 1;
                }
                Ok((
                    start,
                    Tok::Name(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned()),
                ))
            }
            _ => Err(self.err(format!("unexpected character `{}`", c as char))),
        }
    }

    fn number(&mut self) -> PResult<Tok> {
        let start = self.pos;
        if self.src[self.pos] == b'-' {
            self.pos += 1;
        }
        let digits = |l: &mut Self| {
            let s = l.pos;
            while l.pos < l.src.len() && l.src[l.pos].is_ascii_digit() {
                l.pos += 1;
            }
            l.pos > s
        };
        if !digits(self) {
            return Err(self.err("expected digits"));
        }
        let mut float = false;
        if self.src.get(self.pos) == Some(&b'.') {
            float = true;
            self.pos += 1;
            if !digits(self) {
                return Err(self.err("expected digits after `.`"));
            }
        }
        if matches!(self.src.get(self.pos), Some(b'e' | b'E')) {
            float = true;
            self.pos += 1;
            if matches!(self.src.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return Err(self.err("expected exponent digits"));
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap_or("0");
        if float {
            text.parse()
                .map(Tok::Float)
                .map_err(|_| self.err("bad float"))
        } else {
            text.parse().map(Tok::Int).map_err(|_| self.err("bad int"))
        }
    }

    fn string(&mut self) -> PResult<String> {
        if self.src[self.pos..].starts_with(b"\"\"\"") {
            self.pos += 3;
            let mut raw = Vec::new();
            loop {
                if self.pos >= self.src.len() {
                    return Err(self.err("unterminated block string"));
                }
                if self.src[self.pos..].starts_with(b"\\\"\"\"") {
                    raw.extend_from_slice(b"\"\"\"");
                    self.pos += 4;
                } else if self.src[self.pos..].starts_with(b"\"\"\"") {
                    self.pos += 3;
                    break;
                } else {
                    raw.push(self.src[self.pos]);
                    self.pos += 1;
                }
            }
            return Ok(block_string(&String::from_utf8_lossy(&raw)));
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(&c) = self.src.get(self.pos) else {
                return Err(self.err("unterminated string"));
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\n' | b'\r' => return Err(self.err("newline in string")),
                b'\\' => {
                    let e = *self
                        .src
                        .get(self.pos + 1)
                        .ok_or_else(|| self.err("bad escape"))?;
                    self.pos += 2;
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
                            let hex = self
                                .src
                                .get(self.pos..self.pos + 4)
                                .and_then(|h| std::str::from_utf8(h).ok())
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .ok_or_else(|| self.err("bad unicode escape"))?;
                            self.pos += 4;
                            let mut code = hex;
                            if (0xD800..0xDC00).contains(&hex)
                                && self.src[self.pos..].starts_with(b"\\u")
                                && let Some(lo) = self
                                    .src
                                    .get(self.pos + 2..self.pos + 6)
                                    .and_then(|h| std::str::from_utf8(h).ok())
                                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                                && (0xDC00..0xE000).contains(&lo)
                            {
                                code = 0x10000 + ((hex - 0xD800) << 10) + (lo - 0xDC00);
                                self.pos += 6;
                            }
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                _ => {
                    let rest = &self.src[self.pos..];
                    let len = utf8_len(c).min(rest.len());
                    out.push_str(&String::from_utf8_lossy(&rest[..len]));
                    self.pos += len;
                }
            }
        }
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// The block string value: common indentation and blank first/last lines removed.
fn block_string(raw: &str) -> String {
    let lines: Vec<&str> = raw.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let indent = lines
        .iter()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    let mut out: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                l.to_string()
            } else {
                l.get(indent..).unwrap_or("").to_string()
            }
        })
        .collect();
    while out.first().is_some_and(|l| l.trim().is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// A value in a document.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Enum(String),
    List(Vec<Val>),
    Obj(BTreeMap<String, Val>),
    Var(String),
}

/// A directive use.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Directive {
    pub name: String,
    pub args: Vec<(String, Val)>,
}

/// A field selection.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Field {
    pub alias: Option<String>,
    pub name: String,
    pub args: Vec<(String, Val)>,
    pub directives: Vec<Directive>,
    pub sel: Vec<Selection>,
}

impl Field {
    /// The response key.
    pub fn key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// A selection.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Selection {
    Field(Field),
    Spread {
        name: String,
        directives: Vec<Directive>,
    },
    Inline {
        on: Option<String>,
        directives: Vec<Directive>,
        sel: Vec<Selection>,
    },
}

/// A variable definition.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VarDef {
    pub name: String,
    pub default: Option<Val>,
}

/// An operation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Operation {
    pub mutation: bool,
    pub name: Option<String>,
    pub vars: Vec<VarDef>,
    pub sel: Vec<Selection>,
}

/// A fragment definition.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Fragment {
    pub on: String,
    pub sel: Vec<Selection>,
}

/// An executable document.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Document {
    pub ops: Vec<Operation>,
    pub fragments: BTreeMap<String, Fragment>,
}

struct Parser<'a> {
    lex: Lexer<'a>,
    tok: Tok,
    at: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> PResult<Parser<'a>> {
        let mut lex = Lexer {
            src: src.as_bytes(),
            pos: 0,
        };
        let (at, tok) = lex.next()?;
        Ok(Parser { lex, tok, at })
    }

    fn bump(&mut self) -> PResult<Tok> {
        let (at, t) = self.lex.next()?;
        self.at = at;
        Ok(std::mem::replace(&mut self.tok, t))
    }

    fn err<T>(&self, m: impl Into<String>) -> PResult<T> {
        Err(ParseError {
            message: m.into(),
            offset: self.at,
        })
    }

    fn is(&self, c: char) -> bool {
        self.tok == Tok::Punct(c)
    }

    fn eat(&mut self, c: char) -> PResult<bool> {
        if self.is(c) {
            self.bump()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect(&mut self, c: char) -> PResult<()> {
        if self.eat(c)? {
            Ok(())
        } else {
            self.err(format!("expected `{c}`, found {:?}", self.tok))
        }
    }

    fn name(&mut self) -> PResult<String> {
        match self.bump()? {
            Tok::Name(n) => Ok(n),
            t => self.err(format!("expected a name, found {t:?}")),
        }
    }

    fn peek_name(&self, n: &str) -> bool {
        matches!(&self.tok, Tok::Name(x) if x == n)
    }

    fn value(&mut self, constant: bool) -> PResult<Val> {
        match self.bump()? {
            Tok::Punct('$') if !constant => Ok(Val::Var(self.name()?)),
            Tok::Int(i) => Ok(Val::Int(i)),
            Tok::Float(f) => Ok(Val::Float(f)),
            Tok::Str(s) => Ok(Val::Str(s)),
            Tok::Name(n) => Ok(match n.as_str() {
                "true" => Val::Bool(true),
                "false" => Val::Bool(false),
                "null" => Val::Null,
                _ => Val::Enum(n),
            }),
            Tok::Punct('[') => {
                let mut v = Vec::new();
                while !self.eat(']')? {
                    v.push(self.value(constant)?);
                }
                Ok(Val::List(v))
            }
            Tok::Punct('{') => {
                let mut m = BTreeMap::new();
                while !self.eat('}')? {
                    let k = self.name()?;
                    self.expect(':')?;
                    m.insert(k, self.value(constant)?);
                }
                Ok(Val::Obj(m))
            }
            t => self.err(format!("unexpected {t:?} in a value")),
        }
    }

    fn args(&mut self, constant: bool) -> PResult<Vec<(String, Val)>> {
        let mut out = Vec::new();
        if self.eat('(')? {
            while !self.eat(')')? {
                let n = self.name()?;
                self.expect(':')?;
                out.push((n, self.value(constant)?));
            }
        }
        Ok(out)
    }

    fn directives(&mut self) -> PResult<Vec<Directive>> {
        let mut out = Vec::new();
        while self.eat('@')? {
            let name = self.name()?;
            let args = self.args(false)?;
            out.push(Directive { name, args });
        }
        Ok(out)
    }

    fn selection_set(&mut self) -> PResult<Vec<Selection>> {
        self.expect('{')?;
        let mut out = Vec::new();
        while !self.eat('}')? {
            if self.tok == Tok::Spread {
                self.bump()?;
                if self.peek_name("on") {
                    self.bump()?;
                    let on = Some(self.name()?);
                    let directives = self.directives()?;
                    let sel = self.selection_set()?;
                    out.push(Selection::Inline {
                        on,
                        directives,
                        sel,
                    });
                } else if let Tok::Name(_) = &self.tok {
                    let name = self.name()?;
                    let directives = self.directives()?;
                    out.push(Selection::Spread { name, directives });
                } else {
                    let directives = self.directives()?;
                    let sel = self.selection_set()?;
                    out.push(Selection::Inline {
                        on: None,
                        directives,
                        sel,
                    });
                }
                continue;
            }
            let first = self.name()?;
            let (alias, name) = if self.eat(':')? {
                (Some(first), self.name()?)
            } else {
                (None, first)
            };
            let args = self.args(false)?;
            let directives = self.directives()?;
            let sel = if self.is('{') {
                self.selection_set()?
            } else {
                Vec::new()
            };
            out.push(Selection::Field(Field {
                alias,
                name,
                args,
                directives,
                sel,
            }));
        }
        Ok(out)
    }

    fn skip_type(&mut self) -> PResult<()> {
        if self.eat('[')? {
            self.skip_type()?;
            self.expect(']')?;
        } else {
            self.name()?;
        }
        self.eat('!')?;
        Ok(())
    }

    fn document(&mut self) -> PResult<Document> {
        let mut doc = Document::default();
        while self.tok != Tok::Eof {
            if self.is('{') {
                let sel = self.selection_set()?;
                doc.ops.push(Operation {
                    mutation: false,
                    name: None,
                    vars: Vec::new(),
                    sel,
                });
                continue;
            }
            let kw = self.name()?;
            match kw.as_str() {
                "query" | "mutation" | "subscription" => {
                    if kw == "subscription" {
                        return self.err("subscriptions are not supported");
                    }
                    let name = match &self.tok {
                        Tok::Name(_) => Some(self.name()?),
                        _ => None,
                    };
                    let mut vars = Vec::new();
                    if self.eat('(')? {
                        while !self.eat(')')? {
                            self.expect('$')?;
                            let n = self.name()?;
                            self.expect(':')?;
                            self.skip_type()?;
                            let default = if self.eat('=')? {
                                Some(self.value(true)?)
                            } else {
                                None
                            };
                            self.directives()?;
                            vars.push(VarDef { name: n, default });
                        }
                    }
                    self.directives()?;
                    let sel = self.selection_set()?;
                    doc.ops.push(Operation {
                        mutation: kw == "mutation",
                        name,
                        vars,
                        sel,
                    });
                }
                "fragment" => {
                    let name = self.name()?;
                    if !self.peek_name("on") {
                        return self.err("expected `on`");
                    }
                    self.bump()?;
                    let on = self.name()?;
                    self.directives()?;
                    let sel = self.selection_set()?;
                    doc.fragments.insert(name, Fragment { on, sel });
                }
                other => return self.err(format!("unexpected `{other}`")),
            }
        }
        if doc.ops.is_empty() {
            return self.err("the document has no operation");
        }
        Ok(doc)
    }
}

/// Parses an executable document.
pub(crate) fn parse_document(src: &str) -> PResult<Document> {
    Parser::new(src)?.document()
}

/// A type reference in the schema: `Name`, `[T]`, `T!`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TypeRef {
    Named(String),
    List(Box<TypeRef>),
    NonNull(Box<TypeRef>),
}

impl TypeRef {
    /// The innermost named type.
    pub fn base(&self) -> &str {
        match self {
            TypeRef::Named(n) => n,
            TypeRef::List(t) | TypeRef::NonNull(t) => t.base(),
        }
    }
}

/// A field or argument definition.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FieldDef {
    pub name: String,
    pub ty: TypeRef,
    pub args: Vec<(String, TypeRef, Option<Val>)>,
}

/// The kind of a schema type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Scalar,
    Object,
    Interface,
    Union,
    Enum,
    Input,
}

/// A schema type definition.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TypeDef {
    pub name: String,
    pub kind: Kind,
    pub fields: Vec<FieldDef>,
    pub interfaces: Vec<String>,
    pub members: Vec<String>,
    pub values: Vec<String>,
}

impl Parser<'_> {
    fn type_ref(&mut self) -> PResult<TypeRef> {
        let t = if self.eat('[')? {
            let inner = self.type_ref()?;
            self.expect(']')?;
            TypeRef::List(Box::new(inner))
        } else {
            TypeRef::Named(self.name()?)
        };
        Ok(if self.eat('!')? {
            TypeRef::NonNull(Box::new(t))
        } else {
            t
        })
    }

    fn skip_description(&mut self) -> PResult<()> {
        if let Tok::Str(_) = self.tok {
            self.bump()?;
        }
        Ok(())
    }

    fn field_defs(&mut self, input: bool) -> PResult<Vec<FieldDef>> {
        let mut out = Vec::new();
        self.expect('{')?;
        while !self.eat('}')? {
            self.skip_description()?;
            let name = self.name()?;
            let mut args = Vec::new();
            if !input && self.eat('(')? {
                while !self.eat(')')? {
                    self.skip_description()?;
                    let an = self.name()?;
                    self.expect(':')?;
                    let at = self.type_ref()?;
                    let default = if self.eat('=')? {
                        Some(self.value(true)?)
                    } else {
                        None
                    };
                    args.push((an, at, default));
                }
            }
            self.expect(':')?;
            let ty = self.type_ref()?;
            if input && self.eat('=')? {
                self.value(true)?;
            }
            self.directives()?;
            out.push(FieldDef { name, ty, args });
        }
        Ok(out)
    }

    fn schema(&mut self) -> PResult<Vec<TypeDef>> {
        let mut out = Vec::new();
        while self.tok != Tok::Eof {
            self.skip_description()?;
            let kw = self.name()?;
            let name = self.name()?;
            let mut t = TypeDef {
                name,
                kind: Kind::Scalar,
                fields: Vec::new(),
                interfaces: Vec::new(),
                members: Vec::new(),
                values: Vec::new(),
            };
            match kw.as_str() {
                "scalar" => {}
                "type" | "interface" => {
                    t.kind = if kw == "type" {
                        Kind::Object
                    } else {
                        Kind::Interface
                    };
                    if self.peek_name("implements") {
                        self.bump()?;
                        self.eat('&')?;
                        t.interfaces.push(self.name()?);
                        while self.eat('&')? {
                            t.interfaces.push(self.name()?);
                        }
                    }
                    t.fields = self.field_defs(false)?;
                }
                "union" => {
                    t.kind = Kind::Union;
                    self.expect('=')?;
                    self.eat('|')?;
                    t.members.push(self.name()?);
                    while self.eat('|')? {
                        t.members.push(self.name()?);
                    }
                }
                "enum" => {
                    t.kind = Kind::Enum;
                    self.expect('{')?;
                    while !self.eat('}')? {
                        self.skip_description()?;
                        t.values.push(self.name()?);
                    }
                }
                "input" => {
                    t.kind = Kind::Input;
                    t.fields = self.field_defs(true)?;
                }
                other => return self.err(format!("unexpected `{other}` in the schema")),
            }
            out.push(t);
        }
        Ok(out)
    }
}

/// Parses schema definition language.
pub(crate) fn parse_schema(src: &str) -> PResult<Vec<TypeDef>> {
    Parser::new(src)?.schema()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_queries_with_fragments_variables_and_directives() {
        let doc = parse_document(
            r#"
            # comment
            query PullRequestList($owner: String!, $repo: String!, $limit: Int = 30, $state: [PullRequestState!]) {
              repository(owner: $owner, name: $repo) {
                pullRequests(states: $state, first: $limit, orderBy: {field: CREATED_AT, direction: DESC}) {
                  totalCount
                  nodes { ...pr  number @include(if: true) }
                  pageInfo { hasNextPage endCursor }
                }
                alias: issue(number: 1) { ... on Issue { title } ... { id } }
              }
            }
            fragment pr on PullRequest { title body: bodyText }
            "#,
        )
        .unwrap();
        assert_eq!(doc.ops.len(), 1);
        let op = &doc.ops[0];
        assert_eq!(op.name.as_deref(), Some("PullRequestList"));
        assert_eq!(op.vars[2].default, Some(Val::Int(30)));
        assert!(doc.fragments.contains_key("pr"));
        let Selection::Field(repo) = &op.sel[0] else {
            panic!()
        };
        assert_eq!(repo.args[0], ("owner".into(), Val::Var("owner".into())));
        let Selection::Field(prs) = &repo.sel[0] else {
            panic!()
        };
        assert!(
            matches!(&prs.args[2].1, Val::Obj(m) if m["field"] == Val::Enum("CREATED_AT".into()))
        );
        let Selection::Field(alias) = &repo.sel[1] else {
            panic!()
        };
        assert_eq!(alias.key(), "alias");
    }

    #[test]
    fn strings_and_errors() {
        let doc = parse_document(
            r#"{ a(x: "q\"é", y: """
              block
                text
            """) }"#,
        )
        .unwrap();
        let Selection::Field(f) = &doc.ops[0].sel[0] else {
            panic!()
        };
        assert_eq!(f.args[0].1, Val::Str("q\"é".into()));
        assert_eq!(f.args[1].1, Val::Str("block\n  text".into()));
        assert!(parse_document("{ a(").is_err());
        assert!(parse_document("fragment x on Y { a }").is_err());
    }

    #[test]
    fn parses_schema() {
        let s = parse_schema(
            "scalar URI\n\"doc\" type A implements Node & B { id: ID! f(first: Int = 1, x: [String!]): [A!]! }\nunion U = A | C\nenum E { X Y }\ninput I { a: Int = 2 }",
        )
        .unwrap();
        assert_eq!(s[1].interfaces, vec!["Node", "B"]);
        assert_eq!(s[1].fields[1].args.len(), 2);
        assert_eq!(s[1].fields[1].ty.base(), "A");
        assert_eq!(s[2].members, vec!["A", "C"]);
        assert_eq!(s[3].values, vec!["X", "Y"]);
    }
}
