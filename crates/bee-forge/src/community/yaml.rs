//! A small YAML subset parser for the files the community module reads: `FUNDING.yml`,
//! discussion forms, `CITATION.cff` and `release.yml`.
//!
//! Supported: block mappings and sequences (including `- key: value` items), single-line flow
//! sequences and mappings, plain, single- and double-quoted scalars, `|` and `>` block
//! scalars, comments. Not supported: anchors, tags, multi-document streams, multi-line flow
//! collections and complex keys. Unsupported input is an error, never silently misread.

/// A parsed value.
#[derive(Clone, Debug, PartialEq)]
pub enum Yaml {
    /// `null`, `~` or an empty value.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// Any other scalar, as text.
    Str(String),
    /// A sequence.
    Seq(Vec<Yaml>),
    /// A mapping, in file order.
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    /// The value of `key` in a mapping.
    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The text of a scalar (booleans render as `true`/`false`).
    pub fn as_str(&self) -> Option<String> {
        match self {
            Yaml::Str(s) => Some(s.clone()),
            Yaml::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// The items of a sequence (a lone scalar counts as one item).
    pub fn items(&self) -> Vec<&Yaml> {
        match self {
            Yaml::Seq(s) => s.iter().collect(),
            Yaml::Null => Vec::new(),
            other => vec![other],
        }
    }

    /// The boolean value, if a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Yaml::Bool(b) => Some(*b),
            Yaml::Str(s) if s == "yes" => Some(true),
            Yaml::Str(s) if s == "no" => Some(false),
            _ => None,
        }
    }

    /// Every scalar in a sequence as text.
    pub fn strings(&self) -> Vec<String> {
        self.items().into_iter().filter_map(Yaml::as_str).collect()
    }
}

struct Line<'a> {
    indent: usize,
    text: &'a str,
    raw: usize,
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    raw: Vec<&'a str>,
    pos: usize,
}

/// Parses `src`.
pub fn parse(src: &str) -> Result<Yaml, String> {
    let raw: Vec<&str> = src.lines().collect();
    let mut lines = Vec::new();
    for (i, l) in raw.iter().enumerate() {
        let l = l.trim_end_matches('\r');
        if l.contains('\t') && l.trim_start_matches(' ').starts_with('\t') {
            return Err(format!("line {}: tabs cannot indent", i + 1));
        }
        let text = strip_comment(l);
        let trimmed = text.trim_start_matches(' ');
        if trimmed.trim().is_empty() || trimmed == "---" {
            continue;
        }
        lines.push(Line {
            indent: text.len() - trimmed.len(),
            text: trimmed.trim_end(),
            raw: i,
        });
    }
    let mut p = Parser { lines, raw, pos: 0 };
    if p.lines.is_empty() {
        return Ok(Yaml::Null);
    }
    let first = p.lines[0].indent;
    let v = p.block(first)?;
    if p.pos < p.lines.len() {
        return Err(format!(
            "line {}: unexpected content",
            p.lines[p.pos].raw + 1
        ));
    }
    Ok(v)
}

fn strip_comment(l: &str) -> &str {
    let b = l.as_bytes();
    let (mut sq, mut dq) = (false, false);
    for i in 0..b.len() {
        match b[i] {
            b'\'' if !dq => sq = !sq,
            b'"' if !sq => dq = !dq,
            b'#' if !sq && !dq && (i == 0 || b[i - 1] == b' ') => return &l[..i],
            _ => {}
        }
    }
    l
}

fn is_seq_item(t: &str) -> bool {
    t == "-" || t.starts_with("- ")
}

/// Splits `key: value` at the first unquoted `: ` (or trailing `:`).
fn split_key(t: &str) -> Option<(String, &str)> {
    let b = t.as_bytes();
    if b.is_empty() {
        return None;
    }
    if b[0] == b'"' || b[0] == b'\'' {
        let q = b[0];
        let end = t[1..].find(q as char)? + 1;
        let rest = &t[end + 1..];
        let rest = rest.strip_prefix(':')?;
        if !rest.is_empty() && !rest.starts_with(' ') {
            return None;
        }
        return Some((t[1..end].to_string(), rest.trim()));
    }
    if t.starts_with('[') || t.starts_with('{') {
        return None;
    }
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' ') {
            return Some((t[..i].trim().to_string(), t[i + 1..].trim()));
        }
        i += 1;
    }
    None
}

fn scalar(t: &str) -> Yaml {
    let t = t.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let inner = &t[1..t.len() - 1];
        let mut out = String::new();
        let mut it = inner.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                match it.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(o) => out.push(o),
                    None => {}
                }
            } else {
                out.push(c);
            }
        }
        return Yaml::Str(out);
    }
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return Yaml::Str(t[1..t.len() - 1].replace("''", "'"));
    }
    match t {
        "" | "~" | "null" | "Null" | "NULL" => Yaml::Null,
        "true" | "True" | "TRUE" => Yaml::Bool(true),
        "false" | "False" | "FALSE" => Yaml::Bool(false),
        _ => Yaml::Str(t.to_string()),
    }
}

fn split_flow(inner: &str) -> Vec<&str> {
    let (mut depth, mut start, mut sq, mut dq) = (0i32, 0, false, false);
    let mut out = Vec::new();
    for (i, c) in inner.char_indices() {
        match c {
            '\'' if !dq => sq = !sq,
            '"' if !sq => dq = !dq,
            '[' | '{' if !sq && !dq => depth += 1,
            ']' | '}' if !sq && !dq => depth -= 1,
            ',' if depth == 0 && !sq && !dq => {
                out.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if !inner[start..].trim().is_empty() {
        out.push(&inner[start..]);
    }
    out
}

fn value(t: &str) -> Result<Yaml, String> {
    let t = t.trim();
    if let Some(inner) = t.strip_prefix('[') {
        let inner = inner
            .strip_suffix(']')
            .ok_or("unterminated flow sequence")?;
        return split_flow(inner)
            .into_iter()
            .map(value)
            .collect::<Result<Vec<_>, _>>()
            .map(Yaml::Seq);
    }
    if let Some(inner) = t.strip_prefix('{') {
        let inner = inner.strip_suffix('}').ok_or("unterminated flow mapping")?;
        let mut m = Vec::new();
        for part in split_flow(inner) {
            let (k, v) = split_key(part.trim()).ok_or("bad flow mapping entry")?;
            m.push((k, value(v)?));
        }
        return Ok(Yaml::Map(m));
    }
    if t.starts_with('&') || t.starts_with('*') || t.starts_with('!') {
        return Err("anchors, aliases and tags are not supported".into());
    }
    Ok(scalar(t))
}

impl<'a> Parser<'a> {
    fn block(&mut self, indent: usize) -> Result<Yaml, String> {
        let t = self.lines[self.pos].text;
        if is_seq_item(t) {
            self.seq(indent)
        } else {
            self.map(indent)
        }
    }

    fn seq(&mut self, indent: usize) -> Result<Yaml, String> {
        let mut items = Vec::new();
        while self.pos < self.lines.len()
            && self.lines[self.pos].indent == indent
            && is_seq_item(self.lines[self.pos].text)
        {
            let text = self.lines[self.pos].text;
            let rest = text[1..].trim_start();
            let offset = text.len() - rest.len();
            if rest.is_empty() {
                self.pos += 1;
                items.push(self.child(indent)?);
            } else if split_key(rest).is_some() {
                // `- key: value`: the item is a mapping whose first key sits after the dash.
                let line = &mut self.lines[self.pos];
                line.indent = indent + offset;
                line.text = rest;
                items.push(self.map(indent + offset)?);
            } else {
                self.pos += 1;
                items.push(value(rest)?);
            }
        }
        Ok(Yaml::Seq(items))
    }

    fn child(&mut self, parent: usize) -> Result<Yaml, String> {
        match self.lines.get(self.pos) {
            Some(l) if l.indent > parent => {
                let i = l.indent;
                self.block(i)
            }
            _ => Ok(Yaml::Null),
        }
    }

    fn map(&mut self, indent: usize) -> Result<Yaml, String> {
        let mut entries = Vec::new();
        while self.pos < self.lines.len() && self.lines[self.pos].indent == indent {
            let line = &self.lines[self.pos];
            if is_seq_item(line.text) {
                break;
            }
            let (key, rest) = split_key(line.text)
                .ok_or_else(|| format!("line {}: expected `key: value`", line.raw + 1))?;
            let raw_idx = line.raw;
            self.pos += 1;
            let v = if rest.is_empty() {
                // A sequence may sit at the same indent as its key.
                match self.lines.get(self.pos) {
                    Some(l) if l.indent == indent && is_seq_item(l.text) => self.seq(indent)?,
                    _ => self.child(indent)?,
                }
            } else if let Some(style) = block_style(rest) {
                self.block_scalar(raw_idx, indent, style)
            } else {
                value(rest).map_err(|e| format!("line {}: {e}", raw_idx + 1))?
            };
            entries.push((key, v));
        }
        Ok(Yaml::Map(entries))
    }

    fn block_scalar(&mut self, key_raw: usize, indent: usize, style: (char, char)) -> Yaml {
        let mut collected: Vec<&str> = Vec::new();
        let mut i = key_raw + 1;
        let mut block_indent = None;
        while i < self.raw.len() {
            let l = self.raw[i].trim_end_matches('\r');
            if l.trim().is_empty() {
                collected.push("");
                i += 1;
                continue;
            }
            let ind = l.len() - l.trim_start_matches(' ').len();
            if ind <= indent {
                break;
            }
            let bi = *block_indent.get_or_insert(ind);
            if ind < bi {
                break;
            }
            collected.push(&l[bi..]);
            i += 1;
        }
        while collected.last().is_some_and(|l| l.is_empty()) {
            collected.pop();
        }
        while self.pos < self.lines.len() && self.lines[self.pos].raw < i {
            self.pos += 1;
        }
        let text = if style.0 == '|' {
            collected.join("\n")
        } else {
            let mut s = String::new();
            for (n, l) in collected.iter().enumerate() {
                if n > 0 {
                    s.push(if l.is_empty() || collected[n - 1].is_empty() {
                        '\n'
                    } else {
                        ' '
                    });
                }
                s.push_str(l);
            }
            s
        };
        Yaml::Str(if style.1 == '-' {
            text
        } else {
            format!("{text}\n")
        })
    }
}

fn block_style(rest: &str) -> Option<(char, char)> {
    let mut c = rest.chars();
    let first = c.next()?;
    if first != '|' && first != '>' {
        return None;
    }
    let chomp = match c.next() {
        None => '+',
        Some('-') => '-',
        Some(_) => '+',
    };
    Some((first, chomp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_funding_style_files() {
        let y = parse("github: [octocat, \"hubot\"]\nko_fi: someone # c\ncustom:\n  - https://a.example\n  - 'https://b.example'\n").unwrap();
        assert_eq!(y.get("github").unwrap().strings(), vec!["octocat", "hubot"]);
        assert_eq!(y.get("ko_fi").unwrap().as_str().unwrap(), "someone");
        assert_eq!(y.get("custom").unwrap().strings().len(), 2);
    }

    #[test]
    fn parses_forms_and_nested_sequences() {
        let src = "name: Q\nbody:\n  - type: textarea\n    id: what\n    attributes:\n      label: What?\n    validations:\n      required: true\n  - type: markdown\n    attributes:\n      value: |\n        hello\n        world\n";
        let y = parse(src).unwrap();
        let body = y.get("body").unwrap().items();
        assert_eq!(body.len(), 2);
        assert_eq!(body[0].get("id").unwrap().as_str().unwrap(), "what");
        assert_eq!(
            body[0].get("validations").unwrap().get("required"),
            Some(&Yaml::Bool(true))
        );
        assert_eq!(
            body[1]
                .get("attributes")
                .unwrap()
                .get("value")
                .unwrap()
                .as_str()
                .unwrap(),
            "hello\nworld\n"
        );
    }

    #[test]
    fn same_indent_sequences_and_errors() {
        let y = parse("labels:\n- a\n- b\nother: x\n").unwrap();
        assert_eq!(y.get("labels").unwrap().strings(), vec!["a", "b"]);
        assert_eq!(y.get("other").unwrap().as_str().unwrap(), "x");
        assert!(parse("a: &x 1\n").is_err());
        assert!(parse("just text\n").is_err());
    }
}
