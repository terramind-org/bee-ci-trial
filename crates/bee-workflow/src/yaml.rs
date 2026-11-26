//! A YAML reader for workflow files.
//!
//! Workflow files use a well-defined subset of YAML 1.2: block mappings and sequences, flow
//! collections, plain, single- and double-quoted scalars, literal and folded block scalars,
//! comments, a single document (optionally opened by `---`), and anchors with aliases. Every
//! node keeps the line and column where it starts so validation errors can point at it.
//!
//! Plain scalars are resolved with the YAML 1.2 core schema, which is what GitHub uses: `on`,
//! `yes` and `off` are strings, never booleans. Tags, merge keys (`<<`) and multiple documents
//! are rejected with a positioned error, as GitHub does.

use std::collections::BTreeMap;
use std::fmt;

/// A 1-based line and column in the source text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    /// Line number, starting at 1.
    pub line: u32,
    /// Column number (in characters), starting at 1.
    pub col: u32,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

/// How a scalar was written, which decides how it is typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarStyle {
    /// Unquoted; typed by the core schema.
    Plain,
    /// `'single'` quoted; always a string.
    SingleQuoted,
    /// `"double"` quoted; always a string.
    DoubleQuoted,
    /// `|` literal block; always a string.
    Literal,
    /// `>` folded block; always a string.
    Folded,
}

/// A parsed YAML node.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// A scalar with its text and style.
    Scalar(String, ScalarStyle),
    /// A sequence of nodes.
    Seq(Vec<Node>),
    /// A mapping, in source order. Keys are unique.
    Map(Vec<(Node, Node)>),
}

/// A YAML node with its source position.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// What the node holds.
    pub kind: NodeKind,
    /// Where the node starts.
    pub pos: Pos,
}

/// A typed scalar under the YAML 1.2 core schema.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar<'a> {
    /// `null`, `~` or empty.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer or float.
    Number(f64),
    /// Any other text.
    Str(&'a str),
}

impl Node {
    /// The scalar text, if this is a scalar.
    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Scalar(s, _) => Some(s),
            _ => None,
        }
    }

    /// The mapping entries, if this is a mapping.
    pub fn as_map(&self) -> Option<&[(Node, Node)]> {
        match &self.kind {
            NodeKind::Map(m) => Some(m),
            _ => None,
        }
    }

    /// The sequence items, if this is a sequence.
    pub fn as_seq(&self) -> Option<&[Node]> {
        match &self.kind {
            NodeKind::Seq(s) => Some(s),
            _ => None,
        }
    }

    /// Whether this is a null scalar (`null`, `~` or empty, unquoted).
    pub fn is_null(&self) -> bool {
        matches!(self.scalar(), Some(Scalar::Null))
    }

    /// Looks up a mapping value by exact key.
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.as_map()?
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    }

    /// Types a scalar with the YAML 1.2 core schema.
    pub fn scalar(&self) -> Option<Scalar<'_>> {
        match &self.kind {
            NodeKind::Scalar(s, ScalarStyle::Plain) => Some(resolve_plain(s)),
            NodeKind::Scalar(s, _) => Some(Scalar::Str(s)),
            _ => None,
        }
    }

    /// A short description of the node's type for error messages.
    pub fn type_name(&self) -> &'static str {
        match self.scalar() {
            Some(Scalar::Null) => "null",
            Some(Scalar::Bool(_)) => "boolean",
            Some(Scalar::Number(_)) => "number",
            Some(Scalar::Str(_)) => "string",
            None => match self.kind {
                NodeKind::Seq(_) => "sequence",
                _ => "mapping",
            },
        }
    }
}

/// Resolves a plain scalar with the YAML 1.2 core schema.
pub fn resolve_plain(s: &str) -> Scalar<'_> {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Scalar::Null,
        "true" | "True" | "TRUE" => return Scalar::Bool(true),
        "false" | "False" | "FALSE" => return Scalar::Bool(false),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return Scalar::Number(f64::INFINITY);
        }
        "-.inf" | "-.Inf" | "-.INF" => return Scalar::Number(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => return Scalar::Number(f64::NAN),
        _ => {}
    }
    if let Some(hex) = s.strip_prefix("0x") {
        if !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Scalar::Number(radix_value(hex, 16));
        }
        return Scalar::Str(s);
    }
    if let Some(oct) = s.strip_prefix("0o") {
        if !oct.is_empty() && oct.chars().all(|c| ('0'..='7').contains(&c)) {
            return Scalar::Number(radix_value(oct, 8));
        }
        return Scalar::Str(s);
    }
    if is_core_number(s)
        && let Ok(n) = s.parse::<f64>()
    {
        return Scalar::Number(n);
    }
    Scalar::Str(s)
}

fn radix_value(digits: &str, radix: u32) -> f64 {
    digits.chars().fold(0.0, |acc, c| {
        acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
    })
}

/// `[-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?`
fn is_core_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - f;
        if int_digits == 0 && frac_digits == 0 {
            return false;
        }
    } else if int_digits == 0 {
        return false;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
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

/// A YAML syntax error with its position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YamlError {
    /// Where the problem was found.
    pub pos: Pos,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.pos, self.message)
    }
}

impl std::error::Error for YamlError {}

/// Maximum nesting depth accepted, so hostile input cannot exhaust the stack.
const MAX_DEPTH: usize = 128;
/// Maximum number of nodes produced through alias expansion (guards "billion laughs").
const MAX_ALIAS_NODES: usize = 1_000_000;

/// Parses one YAML document. An empty document yields a null scalar at 1:1.
pub fn parse(src: &str) -> Result<Node, YamlError> {
    let mut p = Parser {
        s: src.chars().collect(),
        i: 0,
        line: 1,
        col: 1,
        anchors: BTreeMap::new(),
        depth: 0,
        alias_budget: MAX_ALIAS_NODES,
    };
    p.parse_document()
}

struct Parser {
    s: Vec<char>,
    i: usize,
    line: u32,
    col: u32,
    anchors: BTreeMap<String, Node>,
    depth: usize,
    alias_budget: usize,
}

fn is_flow_indicator(c: char) -> bool {
    matches!(c, ',' | '[' | ']' | '{' | '}')
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.s.get(self.i + n).copied()
    }

    fn pos(&self) -> Pos {
        Pos {
            line: self.line,
            col: self.col,
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, YamlError> {
        Err(YamlError {
            pos: self.pos(),
            message: message.into(),
        })
    }

    fn err_at<T>(pos: Pos, message: impl Into<String>) -> Result<T, YamlError> {
        Err(YamlError {
            pos,
            message: message.into(),
        })
    }

    /// Whether `c` at offset `n` ends a token (whitespace, newline or end).
    fn is_blank_at(&self, n: usize) -> bool {
        matches!(self.peek_at(n), None | Some(' ' | '\t' | '\n' | '\r'))
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
    }

    /// Skips spaces and a trailing comment, stopping before the newline.
    fn skip_spaces_and_comment(&mut self) {
        self.skip_spaces();
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.bump();
            }
        }
    }

    /// Skips whitespace, newlines and comments. Returns whether a newline was crossed.
    fn skip_ws_lines(&mut self) -> bool {
        let mut crossed = false;
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r') => {
                    self.bump();
                }
                Some('\n') => {
                    crossed = true;
                    self.bump();
                }
                Some('#') => {
                    // A comment must start a line or follow whitespace.
                    let prev = if self.i == 0 {
                        None
                    } else {
                        self.s.get(self.i - 1).copied()
                    };
                    if !matches!(prev, None | Some(' ' | '\t' | '\n' | '\r')) {
                        return crossed;
                    }
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.bump();
                    }
                }
                _ => return crossed,
            }
        }
    }

    fn at_document_marker(&self, marker: char) -> bool {
        self.col == 1
            && self.peek() == Some(marker)
            && self.peek_at(1) == Some(marker)
            && self.peek_at(2) == Some(marker)
            && self.is_blank_at(3)
    }

    fn parse_document(&mut self) -> Result<Node, YamlError> {
        if self.peek() == Some('\u{feff}') {
            self.i += 1;
        }
        self.skip_ws_lines();
        // Directives are not used by workflows.
        if self.col == 1 && self.peek() == Some('%') {
            return self.err("YAML directives are not supported");
        }
        if self.at_document_marker('-') {
            for _ in 0..3 {
                self.bump();
            }
            self.skip_ws_lines();
        }
        let node = if self.peek().is_none() || self.at_document_marker('.') {
            Node {
                kind: NodeKind::Scalar(String::new(), ScalarStyle::Plain),
                pos: Pos { line: 1, col: 1 },
            }
        } else {
            self.parse_block_node(-1)?
        };
        self.skip_ws_lines();
        if self.at_document_marker('.') {
            for _ in 0..3 {
                self.bump();
            }
            self.skip_ws_lines();
        }
        if self.at_document_marker('-') {
            return self.err("multiple YAML documents are not supported");
        }
        if self.peek().is_some() {
            return self.err("unexpected content after the document; check the indentation");
        }
        Ok(node)
    }

    fn enter(&mut self) -> Result<(), YamlError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err("YAML nesting is too deep");
        }
        Ok(())
    }

    /// Parses a block node whose content starts at the cursor (already past whitespace),
    /// belonging to a parent at column `parent` (0-based columns compare as `col - 1`).
    fn parse_block_node(&mut self, parent: i64) -> Result<Node, YamlError> {
        self.enter()?;
        let r = self.parse_block_node_inner(parent);
        self.depth -= 1;
        r
    }

    fn parse_block_node_inner(&mut self, parent: i64) -> Result<Node, YamlError> {
        let start = self.pos();
        // Node properties: anchor and tag.
        let mut anchor = None;
        loop {
            match self.peek() {
                Some('&') => {
                    self.bump();
                    let name = self.anchor_name()?;
                    anchor = Some(name);
                    let crossed = self.skip_ws_lines();
                    if crossed && i64::from(self.col) - 1 <= parent {
                        // `key: &a` followed by a dedent: an anchored null.
                        let node = Node {
                            kind: NodeKind::Scalar(String::new(), ScalarStyle::Plain),
                            pos: start,
                        };
                        return Ok(self.register(anchor, node));
                    }
                }
                Some('!') => return self.err("YAML tags are not supported in workflow files"),
                _ => break,
            }
        }
        let node = self.parse_block_content(parent)?;
        Ok(self.register(anchor, node))
    }

    fn register(&mut self, anchor: Option<String>, node: Node) -> Node {
        if let Some(name) = anchor {
            self.anchors.insert(name, node.clone());
        }
        node
    }

    fn anchor_name(&mut self) -> Result<String, YamlError> {
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || is_flow_indicator(c) {
                break;
            }
            name.push(c);
            self.bump();
        }
        if name.is_empty() {
            return self.err("expected an anchor or alias name");
        }
        Ok(name)
    }

    fn parse_alias(&mut self) -> Result<Node, YamlError> {
        let pos = self.pos();
        self.bump();
        let name = self.anchor_name()?;
        let Some(node) = self.anchors.get(&name) else {
            return Self::err_at(pos, format!("unknown alias '*{name}'"));
        };
        let size = count_nodes(node);
        if size > self.alias_budget {
            return Self::err_at(pos, "alias expansion is too large");
        }
        self.alias_budget -= size;
        let mut node = node.clone();
        node.pos = pos;
        Ok(node)
    }

    fn parse_block_content(&mut self, parent: i64) -> Result<Node, YamlError> {
        let col = i64::from(self.col) - 1;
        let c = self.peek();
        if c == Some('-') && self.is_blank_at(1) {
            return self.parse_block_seq(col);
        }
        if c == Some('*') {
            let alias = self.parse_alias()?;
            self.skip_spaces();
            if self.peek() == Some(':') && self.is_blank_at(1) {
                return self.err("aliases cannot be used as mapping keys");
            }
            return Ok(alias);
        }
        if matches!(c, Some('|' | '>')) {
            return self.parse_block_scalar(parent);
        }
        if self.line_has_key()? {
            return self.parse_block_map(col);
        }
        match c {
            Some('[' | '{') => {
                let node = self.parse_flow_node()?;
                self.check_line_end()?;
                Ok(node)
            }
            Some('"') => {
                let node = self.parse_double_quoted(parent)?;
                self.check_line_end()?;
                Ok(node)
            }
            Some('\'') => {
                let node = self.parse_single_quoted(parent)?;
                self.check_line_end()?;
                Ok(node)
            }
            _ => self.parse_plain_block(parent),
        }
    }

    /// After a complete inline value, only a comment or end of line may follow.
    fn check_line_end(&mut self) -> Result<(), YamlError> {
        self.skip_spaces_and_comment();
        match self.peek() {
            None | Some('\n') | Some('\r') => Ok(()),
            Some(':') => self.err("mapping values are not allowed here"),
            Some(c) => self.err(format!("unexpected '{c}' after a value")),
        }
    }

    /// Whether the text from the cursor to end of line is `key:` followed by blank.
    fn line_has_key(&self) -> Result<bool, YamlError> {
        let mut j = self.i;
        let s = &self.s;
        match s.get(j) {
            Some('"') => {
                j += 1;
                while j < s.len() {
                    match s[j] {
                        '\\' => j += 2,
                        '"' => break,
                        '\n' => return Ok(false),
                        _ => j += 1,
                    }
                }
                j += 1;
            }
            Some('\'') => {
                j += 1;
                while j < s.len() {
                    if s[j] == '\'' {
                        if s.get(j + 1) == Some(&'\'') {
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    if s[j] == '\n' {
                        return Ok(false);
                    }
                    j += 1;
                }
                j += 1;
            }
            Some('[' | '{') => return Ok(false),
            Some('?') if matches!(s.get(j + 1), None | Some(' ' | '\n')) => {
                return Ok(false);
            }
            _ => {
                while j < s.len() {
                    let c = s[j];
                    if c == '\n' {
                        return Ok(false);
                    }
                    if c == '#' && j > self.i && matches!(s[j - 1], ' ' | '\t') {
                        return Ok(false);
                    }
                    if c == ':' && matches!(s.get(j + 1), None | Some(' ' | '\t' | '\n' | '\r')) {
                        return Ok(true);
                    }
                    j += 1;
                }
                return Ok(false);
            }
        }
        while matches!(s.get(j), Some(' ' | '\t')) {
            j += 1;
        }
        Ok(s.get(j) == Some(&':') && matches!(s.get(j + 1), None | Some(' ' | '\t' | '\n' | '\r')))
    }

    fn parse_key(&mut self) -> Result<Node, YamlError> {
        let pos = self.pos();
        match self.peek() {
            Some('"') => self.parse_double_quoted(i64::MAX),
            Some('\'') => self.parse_single_quoted(i64::MAX),
            Some('&') => self.err("anchors on mapping keys are not supported"),
            Some('*') => self.err("aliases cannot be used as mapping keys"),
            Some('!') => self.err("YAML tags are not supported in workflow files"),
            _ => {
                let mut text = String::new();
                while let Some(c) = self.peek() {
                    if c == ':' && self.is_blank_at(1) {
                        break;
                    }
                    text.push(c);
                    self.bump();
                }
                let text = text.trim_end().to_owned();
                if let Some(c) = text.chars().next()
                    && matches!(c, '@' | '`' | '%')
                {
                    return Self::err_at(pos, format!("a plain key cannot start with '{c}'"));
                }
                Ok(Node {
                    kind: NodeKind::Scalar(text, ScalarStyle::Plain),
                    pos,
                })
            }
        }
    }

    fn parse_block_map(&mut self, col: i64) -> Result<Node, YamlError> {
        let start = self.pos();
        let mut entries: Vec<(Node, Node)> = Vec::new();
        loop {
            let key = self.parse_key()?;
            if key.as_str() == Some("<<")
                && matches!(key.kind, NodeKind::Scalar(_, ScalarStyle::Plain))
            {
                return Self::err_at(key.pos, "YAML merge keys ('<<') are not supported");
            }
            self.skip_spaces();
            if self.peek() != Some(':') {
                return self.err("expected ':' after a mapping key");
            }
            self.bump();
            if let Some((k, _)) = entries.iter().find(|(k, _)| k.as_str() == key.as_str()) {
                return Self::err_at(
                    key.pos,
                    format!(
                        "duplicate key '{}' (first defined at {})",
                        key.as_str().unwrap_or_default(),
                        k.pos
                    ),
                );
            }
            let value = self.parse_map_value(col)?;
            entries.push((key, value));
            // Find the next key.
            self.skip_ws_lines();
            if self.peek().is_none() || self.at_document_marker('-') || self.at_document_marker('.')
            {
                break;
            }
            let here = i64::from(self.col) - 1;
            if here < col {
                break;
            }
            if here > col {
                return self.err("bad indentation of a mapping entry");
            }
            if self.peek() == Some('-') && self.is_blank_at(1) {
                return self.err("a sequence item cannot appear here; expected a mapping key");
            }
            if !self.line_has_key()? {
                return self.err("expected a mapping key ('key: value')");
            }
        }
        Ok(Node {
            kind: NodeKind::Map(entries),
            pos: start,
        })
    }

    /// Parses the value after `key:` in a block mapping at column `col`.
    fn parse_map_value(&mut self, col: i64) -> Result<Node, YamlError> {
        self.skip_spaces();
        let inline_pos = self.pos();
        if self.peek() == Some('#') || matches!(self.peek(), None | Some('\n' | '\r')) {
            self.skip_spaces_and_comment();
            // The value is on following lines, or null.
            let save = (self.i, self.line, self.col);
            self.skip_ws_lines();
            let here = i64::from(self.col) - 1;
            let is_seq_same_col = here == col && self.peek() == Some('-') && self.is_blank_at(1);
            if self.peek().is_some()
                && !self.at_document_marker('-')
                && !self.at_document_marker('.')
                && (here > col || is_seq_same_col)
            {
                return self.parse_block_node(if is_seq_same_col { col - 1 } else { col });
            }
            (self.i, self.line, self.col) = save;
            return Ok(Node {
                kind: NodeKind::Scalar(String::new(), ScalarStyle::Plain),
                pos: inline_pos,
            });
        }
        if self.peek() == Some('-') && self.is_blank_at(1) {
            return self.err("a block sequence cannot start on the same line as its key");
        }
        if self.line_has_key()? && !matches!(self.peek(), Some('&' | '*' | '|' | '>')) {
            return self
                .err("mapping values are not allowed here; nested mappings need a new line");
        }
        self.parse_block_node(col)
    }

    fn parse_block_seq(&mut self, col: i64) -> Result<Node, YamlError> {
        let start = self.pos();
        let mut items = Vec::new();
        loop {
            // At '-'.
            let dash = self.pos();
            self.bump();
            self.skip_spaces();
            let item =
                if self.peek() == Some('#') || matches!(self.peek(), None | Some('\n' | '\r')) {
                    self.skip_spaces_and_comment();
                    let save = (self.i, self.line, self.col);
                    self.skip_ws_lines();
                    let here = i64::from(self.col) - 1;
                    if self.peek().is_some() && here > col && !self.at_document_marker('-') {
                        self.parse_block_node(col)?
                    } else {
                        (self.i, self.line, self.col) = save;
                        Node {
                            kind: NodeKind::Scalar(String::new(), ScalarStyle::Plain),
                            pos: dash,
                        }
                    }
                } else {
                    self.parse_block_node(col)?
                };
            items.push(item);
            self.skip_ws_lines();
            if self.peek().is_none() || self.at_document_marker('-') || self.at_document_marker('.')
            {
                break;
            }
            let here = i64::from(self.col) - 1;
            if here < col {
                break;
            }
            if here > col {
                return self.err("bad indentation of a sequence item");
            }
            if !(self.peek() == Some('-') && self.is_blank_at(1)) {
                if self.line_has_key()? {
                    // A mapping key at the same column as the sequence ends a value sequence
                    // that sits at its parent key's column.
                    break;
                }
                return self.err("expected a sequence item ('- ')");
            }
        }
        Ok(Node {
            kind: NodeKind::Seq(items),
            pos: start,
        })
    }

    fn parse_plain_block(&mut self, parent: i64) -> Result<Node, YamlError> {
        let pos = self.pos();
        if let Some(c) = self.peek() {
            if matches!(c, '@' | '`' | '%') {
                return self.err(format!("a plain scalar cannot start with '{c}'"));
            }
            if matches!(c, ']' | '}' | ',') {
                return self.err(format!("unexpected '{c}'"));
            }
        }
        let mut text = self.plain_line(false);
        // Continuation lines.
        loop {
            let save = (self.i, self.line, self.col);
            let mut blank_lines = 0usize;
            // Move to next line.
            self.skip_spaces_and_comment_if_after_space();
            if self.peek() != Some('\n') && self.peek() != Some('\r') {
                break;
            }
            let mut ok = false;
            while matches!(self.peek(), Some('\n' | '\r')) {
                if self.bump() == Some('\n') {
                    // Count blank lines.
                    self.skip_spaces();
                    if matches!(self.peek(), Some('\n' | '\r')) {
                        blank_lines += 1;
                        continue;
                    }
                    ok = true;
                    break;
                }
            }
            let here = i64::from(self.col) - 1;
            if !ok
                || self.peek().is_none()
                || here <= parent
                || self.peek() == Some('#')
                || self.at_document_marker('-')
                || self.at_document_marker('.')
            {
                (self.i, self.line, self.col) = save;
                break;
            }
            if self.line_has_key()? {
                return self
                    .err("mapping values are not allowed here (in a multi-line plain scalar)");
            }
            if self.peek() == Some('-') && self.is_blank_at(1) {
                (self.i, self.line, self.col) = save;
                break;
            }
            let more = self.plain_line(false);
            if blank_lines == 0 {
                text.push(' ');
            } else {
                for _ in 0..blank_lines {
                    text.push('\n');
                }
            }
            text.push_str(&more);
        }
        Ok(Node {
            kind: NodeKind::Scalar(text, ScalarStyle::Plain),
            pos,
        })
    }

    fn skip_spaces_and_comment_if_after_space(&mut self) {
        self.skip_spaces();
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.bump();
            }
        }
    }

    /// Reads plain scalar text to the end of the line (or a flow indicator in flow context).
    fn plain_line(&mut self, flow: bool) -> String {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c == '\n' || c == '\r' {
                break;
            }
            if c == ':'
                && (self.is_blank_at(1)
                    || (flow && matches!(self.peek_at(1), Some(c2) if is_flow_indicator(c2))))
            {
                break;
            }
            if c == '#' && text.ends_with([' ', '\t']) {
                break;
            }
            if flow && is_flow_indicator(c) {
                break;
            }
            text.push(c);
            self.bump();
        }
        text.trim_end().to_owned()
    }

    fn parse_block_scalar(&mut self, parent: i64) -> Result<Node, YamlError> {
        let pos = self.pos();
        let folded = self.bump() == Some('>');
        let mut chomp = 0i8; // -1 strip, 0 clip, 1 keep
        let mut explicit: Option<i64> = None;
        for _ in 0..2 {
            match self.peek() {
                Some('-') => {
                    chomp = -1;
                    self.bump();
                }
                Some('+') => {
                    chomp = 1;
                    self.bump();
                }
                Some(d @ '1'..='9') => {
                    explicit = Some(i64::from(d.to_digit(10).unwrap_or(1)));
                    self.bump();
                }
                _ => {}
            }
        }
        self.skip_spaces_and_comment();
        match self.peek() {
            None => {}
            Some('\n') => {
                self.bump();
            }
            Some('\r') => {
                self.bump();
                if self.peek() == Some('\n') {
                    self.bump();
                }
            }
            Some(c) => return self.err(format!("unexpected '{c}' after a block scalar indicator")),
        }
        let base = parent.max(-1) + 1;
        // Collect raw lines.
        let mut lines: Vec<(i64, String)> = Vec::new(); // (indent, text after indent) ; indent -1 for blank
        let mut indent: Option<i64> = explicit.map(|e| base + e - 1);
        loop {
            if self.peek().is_none() {
                break;
            }
            let line_start = (self.i, self.line, self.col);
            let mut n = 0i64;
            while self.peek() == Some(' ') {
                n += 1;
                self.bump();
            }
            let blank = matches!(self.peek(), None | Some('\n' | '\r'));
            if blank {
                // Keep any spaces beyond the content indentation.
                let extra = match indent {
                    Some(ind) if n > ind => " ".repeat((n - ind) as usize),
                    _ => String::new(),
                };
                lines.push((-1, extra));
                if self.peek() == Some('\r') {
                    self.bump();
                }
                if self.bump().is_none() {
                    break;
                }
                continue;
            }
            let ind = match indent {
                Some(ind) => ind,
                None => {
                    if n <= parent {
                        (self.i, self.line, self.col) = line_start;
                        break;
                    }
                    indent = Some(n);
                    n
                }
            };
            if n < ind
                || (self.col == 1 && (self.at_document_marker('-') || self.at_document_marker('.')))
            {
                (self.i, self.line, self.col) = line_start;
                break;
            }
            let mut text = " ".repeat((n - ind) as usize);
            while let Some(c) = self.peek() {
                if c == '\n' {
                    break;
                }
                text.push(c);
                self.bump();
            }
            if text.ends_with('\r') {
                text.pop();
            }
            lines.push((n - ind, text));
            if self.peek() == Some('\n') {
                self.bump();
            }
        }
        // Trailing blank lines: rewind the cursor so that following structure parsing works.
        // (The cursor is already at the first non-content line.)
        let mut content_end = lines.len();
        while content_end > 0 && lines[content_end - 1].0 < 0 {
            content_end -= 1;
        }
        let trailing_blanks = lines.len() - content_end;
        let body = &lines[..content_end];
        let mut out = String::new();
        if folded {
            let mut prev_more = false;
            let mut first = true;
            let mut pending_newlines = 0usize;
            for (ind, text) in body {
                if *ind < 0 {
                    pending_newlines += 1;
                    continue;
                }
                let more = *ind > 0 || text.starts_with('\t');
                if first {
                    for _ in 0..pending_newlines {
                        out.push('\n');
                    }
                } else if pending_newlines == 0 {
                    if more || prev_more {
                        out.push('\n');
                    } else {
                        out.push(' ');
                    }
                } else {
                    let n = if more || prev_more {
                        pending_newlines + 1
                    } else {
                        pending_newlines
                    };
                    for _ in 0..n {
                        out.push('\n');
                    }
                }
                out.push_str(text);
                pending_newlines = 0;
                prev_more = more;
                first = false;
            }
        } else {
            for (k, (_, text)) in body.iter().enumerate() {
                if k > 0 {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        if !body.is_empty() {
            match chomp {
                -1 => {}
                0 => out.push('\n'),
                _ => {
                    out.push('\n');
                    for _ in 0..trailing_blanks {
                        out.push('\n');
                    }
                }
            }
        } else if chomp == 1 {
            for _ in 0..trailing_blanks {
                out.push('\n');
            }
        }
        let style = if folded {
            ScalarStyle::Folded
        } else {
            ScalarStyle::Literal
        };
        Ok(Node {
            kind: NodeKind::Scalar(out, style),
            pos,
        })
    }

    /// Folds a line break inside a quoted scalar: returns the text to insert.
    fn fold_quoted_break(&mut self, parent: i64) -> Result<String, YamlError> {
        // At '\n' (or '\r').
        let mut newlines = 0usize;
        loop {
            match self.peek() {
                Some('\r') => {
                    self.bump();
                }
                Some('\n') => {
                    self.bump();
                    newlines += 1;
                    while matches!(self.peek(), Some(' ' | '\t')) {
                        self.bump();
                    }
                }
                _ => break,
            }
        }
        if self.peek().is_none() {
            return self.err("unterminated quoted scalar");
        }
        if parent != i64::MAX && i64::from(self.col) - 1 <= parent && parent >= 0 {
            return self.err("continuation of a quoted scalar must be indented");
        }
        if self.at_document_marker('-') || self.at_document_marker('.') {
            return self.err("unterminated quoted scalar");
        }
        Ok(if newlines <= 1 {
            " ".to_owned()
        } else {
            "\n".repeat(newlines - 1)
        })
    }

    fn parse_single_quoted(&mut self, parent: i64) -> Result<Node, YamlError> {
        let pos = self.pos();
        self.bump();
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Self::err_at(pos, "unterminated single-quoted scalar"),
                Some('\'') => {
                    if self.peek_at(1) == Some('\'') {
                        out.push('\'');
                        self.bump();
                        self.bump();
                    } else {
                        self.bump();
                        break;
                    }
                }
                Some('\n' | '\r') => {
                    while out.ends_with([' ', '\t']) {
                        out.pop();
                    }
                    let f = self.fold_quoted_break(parent)?;
                    out.push_str(&f);
                }
                Some(c) => {
                    out.push(c);
                    self.bump();
                }
            }
        }
        Ok(Node {
            kind: NodeKind::Scalar(out, ScalarStyle::SingleQuoted),
            pos,
        })
    }

    fn parse_double_quoted(&mut self, parent: i64) -> Result<Node, YamlError> {
        let pos = self.pos();
        self.bump();
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Self::err_at(pos, "unterminated double-quoted scalar"),
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    let esc_pos = self.pos();
                    self.bump();
                    let Some(e) = self.bump() else {
                        return Self::err_at(pos, "unterminated double-quoted scalar");
                    };
                    match e {
                        '0' => out.push('\0'),
                        'a' => out.push('\u{7}'),
                        'b' => out.push('\u{8}'),
                        't' | '\t' => out.push('\t'),
                        'n' => out.push('\n'),
                        'v' => out.push('\u{b}'),
                        'f' => out.push('\u{c}'),
                        'r' => out.push('\r'),
                        'e' => out.push('\u{1b}'),
                        ' ' => out.push(' '),
                        '"' => out.push('"'),
                        '/' => out.push('/'),
                        '\\' => out.push('\\'),
                        'N' => out.push('\u{85}'),
                        '_' => out.push('\u{a0}'),
                        'L' => out.push('\u{2028}'),
                        'P' => out.push('\u{2029}'),
                        'x' | 'u' | 'U' => {
                            let n = match e {
                                'x' => 2,
                                'u' => 4,
                                _ => 8,
                            };
                            let mut v = 0u32;
                            for _ in 0..n {
                                let Some(d) = self.bump().and_then(|c| c.to_digit(16)) else {
                                    return Self::err_at(esc_pos, "invalid hex escape");
                                };
                                v = v * 16 + d;
                            }
                            let Some(ch) = char::from_u32(v) else {
                                return Self::err_at(esc_pos, "escape is not a valid character");
                            };
                            out.push(ch);
                        }
                        '\n' | '\r' => {
                            if e == '\r' && self.peek() == Some('\n') {
                                self.bump();
                            }
                            // Escaped line break: join without a space.
                            while matches!(self.peek(), Some(' ' | '\t')) {
                                self.bump();
                            }
                            while matches!(self.peek(), Some('\n' | '\r')) {
                                if self.bump() == Some('\n') {
                                    out.push('\n');
                                }
                                while matches!(self.peek(), Some(' ' | '\t')) {
                                    self.bump();
                                }
                            }
                        }
                        other => {
                            return Self::err_at(esc_pos, format!("unknown escape '\\{other}'"));
                        }
                    }
                }
                Some('\n' | '\r') => {
                    while out.ends_with([' ', '\t']) {
                        out.pop();
                    }
                    let f = self.fold_quoted_break(parent)?;
                    out.push_str(&f);
                }
                Some(c) => {
                    out.push(c);
                    self.bump();
                }
            }
        }
        Ok(Node {
            kind: NodeKind::Scalar(out, ScalarStyle::DoubleQuoted),
            pos,
        })
    }

    fn skip_flow_ws(&mut self) -> Result<(), YamlError> {
        self.skip_ws_lines();
        if self.peek().is_none() {
            return self.err("unterminated flow collection");
        }
        Ok(())
    }

    fn parse_flow_node(&mut self) -> Result<Node, YamlError> {
        self.enter()?;
        let r = self.parse_flow_node_inner();
        self.depth -= 1;
        r
    }

    fn parse_flow_node_inner(&mut self) -> Result<Node, YamlError> {
        let mut anchor = None;
        if self.peek() == Some('&') {
            self.bump();
            anchor = Some(self.anchor_name()?);
            self.skip_flow_ws()?;
        }
        let node = match self.peek() {
            Some('[') => self.parse_flow_seq()?,
            Some('{') => self.parse_flow_map()?,
            Some('"') => self.parse_double_quoted(i64::MAX)?,
            Some('\'') => self.parse_single_quoted(i64::MAX)?,
            Some('*') => self.parse_alias()?,
            Some('!') => return self.err("YAML tags are not supported in workflow files"),
            Some(c) if c == ']' || c == '}' || c == ',' => Node {
                kind: NodeKind::Scalar(String::new(), ScalarStyle::Plain),
                pos: self.pos(),
            },
            _ => self.parse_plain_flow()?,
        };
        Ok(self.register(anchor, node))
    }

    fn parse_plain_flow(&mut self) -> Result<Node, YamlError> {
        let pos = self.pos();
        if let Some(c) = self.peek()
            && matches!(c, '@' | '`' | '%')
        {
            return self.err(format!("a plain scalar cannot start with '{c}'"));
        }
        let mut text = self.plain_line(true);
        loop {
            let save = (self.i, self.line, self.col);
            self.skip_spaces_and_comment_if_after_space();
            if !matches!(self.peek(), Some('\n' | '\r')) {
                (self.i, self.line, self.col) = save;
                break;
            }
            let mut newlines = 0;
            while let Some(c) = self.peek() {
                match c {
                    '\n' => {
                        newlines += 1;
                        self.bump();
                    }
                    ' ' | '\t' | '\r' => {
                        self.bump();
                    }
                    _ => break,
                }
            }
            match self.peek() {
                None => return self.err("unterminated flow collection"),
                Some(c) if is_flow_indicator(c) || c == '#' => {
                    break;
                }
                Some(':') if self.is_blank_at(1) => break,
                _ => {}
            }
            let more = self.plain_line(true);
            if newlines <= 1 {
                text.push(' ');
            } else {
                for _ in 1..newlines {
                    text.push('\n');
                }
            }
            text.push_str(&more);
        }
        Ok(Node {
            kind: NodeKind::Scalar(text, ScalarStyle::Plain),
            pos,
        })
    }

    fn parse_flow_seq(&mut self) -> Result<Node, YamlError> {
        let pos = self.pos();
        self.bump();
        let mut items = Vec::new();
        loop {
            self.skip_flow_ws()?;
            if self.peek() == Some(']') {
                self.bump();
                break;
            }
            let item_pos = self.pos();
            let item = self.parse_flow_node()?;
            self.skip_flow_ws()?;
            // A single-pair mapping inside a flow sequence: `[a: b]`.
            let item = if self.peek() == Some(':') {
                self.bump();
                self.skip_flow_ws()?;
                let value = if matches!(self.peek(), Some(',' | ']')) {
                    Node {
                        kind: NodeKind::Scalar(String::new(), ScalarStyle::Plain),
                        pos: self.pos(),
                    }
                } else {
                    self.parse_flow_node()?
                };
                self.skip_flow_ws()?;
                Node {
                    kind: NodeKind::Map(vec![(item, value)]),
                    pos: item_pos,
                }
            } else {
                item
            };
            items.push(item);
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                Some(c) => {
                    return self.err(format!(
                        "expected ',' or ']' in a flow sequence, found '{c}'"
                    ));
                }
                None => return self.err("unterminated flow sequence"),
            }
        }
        Ok(Node {
            kind: NodeKind::Seq(items),
            pos,
        })
    }

    fn parse_flow_map(&mut self) -> Result<Node, YamlError> {
        let pos = self.pos();
        self.bump();
        let mut entries: Vec<(Node, Node)> = Vec::new();
        loop {
            self.skip_flow_ws()?;
            if self.peek() == Some('}') {
                self.bump();
                break;
            }
            let key = self.parse_flow_node()?;
            if !matches!(key.kind, NodeKind::Scalar(..)) {
                return Self::err_at(key.pos, "mapping keys must be scalars");
            }
            self.skip_flow_ws()?;
            let value = if self.peek() == Some(':') {
                self.bump();
                self.skip_flow_ws()?;
                if matches!(self.peek(), Some(',' | '}')) {
                    Node {
                        kind: NodeKind::Scalar(String::new(), ScalarStyle::Plain),
                        pos: self.pos(),
                    }
                } else {
                    self.parse_flow_node()?
                }
            } else {
                Node {
                    kind: NodeKind::Scalar(String::new(), ScalarStyle::Plain),
                    pos: self.pos(),
                }
            };
            if let Some((k, _)) = entries.iter().find(|(k, _)| k.as_str() == key.as_str()) {
                return Self::err_at(
                    key.pos,
                    format!(
                        "duplicate key '{}' (first defined at {})",
                        key.as_str().unwrap_or_default(),
                        k.pos
                    ),
                );
            }
            entries.push((key, value));
            self.skip_flow_ws()?;
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    break;
                }
                Some(c) => {
                    return self.err(format!(
                        "expected ',' or '}}' in a flow mapping, found '{c}'"
                    ));
                }
                None => return self.err("unterminated flow mapping"),
            }
        }
        Ok(Node {
            kind: NodeKind::Map(entries),
            pos,
        })
    }
}

fn count_nodes(node: &Node) -> usize {
    match &node.kind {
        NodeKind::Scalar(..) => 1,
        NodeKind::Seq(items) => 1 + items.iter().map(count_nodes).sum::<usize>(),
        NodeKind::Map(entries) => {
            1 + entries
                .iter()
                .map(|(k, v)| count_nodes(k) + count_nodes(v))
                .sum::<usize>()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: &Node) -> &str {
        n.as_str().unwrap()
    }

    #[test]
    fn on_is_a_string_key_not_a_boolean() {
        let n = parse("on: push\nyes: no\n").unwrap();
        assert_eq!(s(n.get("on").unwrap()), "push");
        assert_eq!(n.get("yes").unwrap().scalar(), Some(Scalar::Str("no")));
    }

    #[test]
    fn nested_block_structures_and_positions() {
        let src = "jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo hi\n        name: Say\n";
        let n = parse(src).unwrap();
        let build = n.get("jobs").unwrap().get("build").unwrap();
        assert_eq!(build.pos, Pos { line: 3, col: 5 });
        let steps = build.get("steps").unwrap().as_seq().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(s(steps[1].get("name").unwrap()), "Say");
        assert_eq!(steps[1].pos, Pos { line: 6, col: 9 });
    }

    #[test]
    fn sequence_at_key_column() {
        let n = parse("branches:\n- main\n- dev\nother: 1\n").unwrap();
        let b = n.get("branches").unwrap().as_seq().unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(n.get("other").unwrap().scalar(), Some(Scalar::Number(1.0)));
    }

    #[test]
    fn flow_collections() {
        let n = parse("a: [ main, 'release/v[0-9]', {x: 1, y: [2, 3]} ]\nb: {}\nc: []\n").unwrap();
        let a = n.get("a").unwrap().as_seq().unwrap();
        assert_eq!(s(&a[0]), "main");
        assert_eq!(s(&a[1]), "release/v[0-9]");
        assert_eq!(a[2].get("y").unwrap().as_seq().unwrap().len(), 2);
        assert!(n.get("b").unwrap().as_map().unwrap().is_empty());
        assert!(n.get("c").unwrap().as_seq().unwrap().is_empty());
    }

    #[test]
    fn multiline_flow_sequence() {
        let n = parse("os: [\n  ubuntu-latest,\n  macos-latest, # comment\n  windows-latest\n]\n")
            .unwrap();
        assert_eq!(n.get("os").unwrap().as_seq().unwrap().len(), 3);
    }

    #[test]
    fn literal_and_folded_scalars() {
        let n = parse("a: |\n  echo 1\n  echo 2\n\nb: >\n  one\n  two\n\n  three\nc: |-\n  x\nd: |+\n  y\n\ne: end\n").unwrap();
        assert_eq!(s(n.get("a").unwrap()), "echo 1\necho 2\n");
        assert_eq!(s(n.get("b").unwrap()), "one two\nthree\n");
        assert_eq!(s(n.get("c").unwrap()), "x");
        assert_eq!(s(n.get("d").unwrap()), "y\n\n");
        assert_eq!(s(n.get("e").unwrap()), "end");
    }

    #[test]
    fn literal_keeps_relative_indentation() {
        let n = parse("run: |\n  if x; then\n    y\n  fi\n").unwrap();
        assert_eq!(s(n.get("run").unwrap()), "if x; then\n  y\nfi\n");
    }

    #[test]
    fn folded_more_indented_lines_keep_newlines() {
        let n = parse("a: >\n  p\n    code\n  q\n").unwrap();
        assert_eq!(s(n.get("a").unwrap()), "p\n  code\nq\n");
    }

    #[test]
    fn quoted_scalars() {
        let n = parse("a: 'it''s'\nb: \"tab\\tnew\\nq\\\"\\u00e9\"\nc: 'multi\n  line'\n").unwrap();
        assert_eq!(s(n.get("a").unwrap()), "it's");
        assert_eq!(s(n.get("b").unwrap()), "tab\tnew\nq\"é");
        assert_eq!(s(n.get("c").unwrap()), "multi line");
        assert_eq!(n.get("a").unwrap().scalar(), Some(Scalar::Str("it's")));
    }

    #[test]
    fn plain_multiline_folds() {
        let n =
            parse("if: github.event_name == 'push' &&\n  github.ref == 'refs/heads/main'\nx: 1\n")
                .unwrap();
        assert_eq!(
            s(n.get("if").unwrap()),
            "github.event_name == 'push' && github.ref == 'refs/heads/main'"
        );
    }

    #[test]
    fn core_schema_typing() {
        let n = parse(
            "a: 3.10\nb: 010\nc: 0x1F\nd: ~\ne: True\nf: '1'\ng: 1e3\nh: v1.0\ni: .5\nj: -7\n",
        )
        .unwrap();
        assert_eq!(n.get("a").unwrap().scalar(), Some(Scalar::Number(3.1)));
        assert_eq!(n.get("b").unwrap().scalar(), Some(Scalar::Number(10.0)));
        assert_eq!(n.get("c").unwrap().scalar(), Some(Scalar::Number(31.0)));
        assert_eq!(n.get("d").unwrap().scalar(), Some(Scalar::Null));
        assert_eq!(n.get("e").unwrap().scalar(), Some(Scalar::Bool(true)));
        assert_eq!(n.get("f").unwrap().scalar(), Some(Scalar::Str("1")));
        assert_eq!(n.get("g").unwrap().scalar(), Some(Scalar::Number(1000.0)));
        assert_eq!(n.get("h").unwrap().scalar(), Some(Scalar::Str("v1.0")));
        assert_eq!(n.get("i").unwrap().scalar(), Some(Scalar::Number(0.5)));
        assert_eq!(n.get("j").unwrap().scalar(), Some(Scalar::Number(-7.0)));
    }

    #[test]
    fn anchors_and_aliases() {
        let n = parse("x: &steps\n  - run: a\ny: *steps\nz: &v hello\nw: *v\n").unwrap();
        assert_eq!(n.get("y").unwrap().as_seq().unwrap().len(), 1);
        assert_eq!(s(n.get("w").unwrap()), "hello");
        assert_eq!(n.get("w").unwrap().pos, Pos { line: 5, col: 4 });
    }

    #[test]
    fn rejects_merge_keys_tags_and_unknown_aliases() {
        assert!(
            parse("a: &x {b: 1}\nc:\n  <<: *x\n")
                .unwrap_err()
                .message
                .contains("merge")
        );
        assert!(parse("a: !!str 1\n").unwrap_err().message.contains("tags"));
        let e = parse("a: *nope\n").unwrap_err();
        assert_eq!(e.pos, Pos { line: 1, col: 4 });
    }

    #[test]
    fn duplicate_keys_are_errors_with_position() {
        let e = parse("a: 1\nb: 2\na: 3\n").unwrap_err();
        assert_eq!(e.pos, Pos { line: 3, col: 1 });
        assert!(e.message.contains("duplicate key 'a'"));
    }

    #[test]
    fn bad_indentation_is_positioned() {
        let e = parse("a:\n  b: 1\n   c: 2\n").unwrap_err();
        assert_eq!(e.pos.line, 3);
        let e = parse("paths:\n  - **/README.md\n").unwrap_err();
        assert_eq!(e.pos.line, 2);
    }

    #[test]
    fn unquoted_bracket_in_flow_is_an_error() {
        // GitHub documents this as a parse error (filter pattern cheat sheet).
        assert!(parse("branches: [ main, release/v[0-9].[0-9] ]\n").is_err());
    }

    #[test]
    fn document_markers_and_comments() {
        let n = parse("# c\n---\nname: CI # trailing\nurl: http://x/#y\n...\n").unwrap();
        assert_eq!(s(n.get("name").unwrap()), "CI");
        assert_eq!(s(n.get("url").unwrap()), "http://x/#y");
        assert!(parse("a: 1\n---\nb: 2\n").is_err());
    }

    #[test]
    fn compact_nested_sequences_and_maps() {
        let n = parse("- - a\n  - b\n- k: v\n  k2: v2\n-\n").unwrap();
        let items = n.as_seq().unwrap();
        assert_eq!(items[0].as_seq().unwrap().len(), 2);
        assert_eq!(s(items[1].get("k2").unwrap()), "v2");
        assert!(items[2].is_null());
    }

    #[test]
    fn empty_values_are_null() {
        let n = parse("on:\n  push:\n  pull_request:\n").unwrap();
        assert!(n.get("on").unwrap().get("push").unwrap().is_null());
        assert!(parse("").unwrap().is_null());
    }

    #[test]
    fn deep_nesting_is_rejected_not_overflowed() {
        let src = "[".repeat(500) + &"]".repeat(500);
        assert!(parse(&src).is_err());
    }
}
