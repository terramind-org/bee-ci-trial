//! A small YAML reader for lockfiles (`pnpm-lock.yaml`, Yarn Berry's `yarn.lock`).
//!
//! It supports the subset those files use: block mappings and sequences, quoted and plain
//! scalars, flow `{}` and `[]` collections, comments and `|`/`>` block scalars. Anchors, tags,
//! multi-document streams and multi-line plain scalars are not supported. Values come back as
//! `serde_json::Value`: `true`/`false` become booleans, everything else a string.

use serde_json::{Map, Value};

use crate::error::{DepsError, Result};

struct Line {
    indent: usize,
    text: String,
    number: usize,
}

/// Parses a YAML document into a JSON value. An empty document is `Null`.
pub fn parse(path: &str, text: &str) -> Result<Value> {
    let mut lines = Vec::new();
    let raw: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < raw.len() {
        let l = raw[i];
        let number = i + 1;
        i += 1;
        let trimmed_end = l.trim_end();
        let content = trimmed_end.trim_start();
        if content.is_empty() || content.starts_with('#') || content == "---" || content == "..." {
            continue;
        }
        let indent = trimmed_end.len() - content.len();
        let cleaned = strip_comment(content);
        // Block scalar: gather the more-indented lines verbatim.
        if let Some(key_part) = block_scalar_header(&cleaned) {
            let mut body: Vec<String> = Vec::new();
            let mut base: Option<usize> = None;
            while i < raw.len() {
                let nxt = raw[i];
                if nxt.trim().is_empty() {
                    body.push(String::new());
                    i += 1;
                    continue;
                }
                let ind = nxt.len() - nxt.trim_start().len();
                if ind <= indent {
                    break;
                }
                let b = *base.get_or_insert(ind);
                body.push(nxt[b.min(ind)..].trim_end().to_owned());
                i += 1;
            }
            while body.last().is_some_and(String::is_empty) {
                body.pop();
            }
            let joined = body.join("\n");
            lines.push(Line {
                indent,
                text: format!("{key_part} {}", quote(&joined)),
                number,
            });
            continue;
        }
        lines.push(Line {
            indent,
            text: cleaned,
            number,
        });
    }
    if lines.is_empty() {
        return Ok(Value::Null);
    }
    let mut idx = 0;
    let first_indent = lines[0].indent;
    let v = parse_node(path, &mut lines, &mut idx, first_indent)?;
    if idx < lines.len() {
        return Err(DepsError::invalid(
            path,
            format!("line {}: unexpected indentation", lines[idx].number),
        ));
    }
    Ok(v)
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

fn block_scalar_header(text: &str) -> Option<String> {
    let t = text.trim_end();
    for suffix in ["|", ">", "|-", ">-", "|+", ">+"] {
        if let Some(head) = t.strip_suffix(suffix)
            && (head.ends_with(':') || head.ends_with(": ") || head == "-" || head.ends_with("- "))
        {
            return Some(head.trim_end().to_owned());
        }
    }
    None
}

fn strip_comment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    let mut prev = ' ';
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => {
                if (c == '\'' || c == '"')
                    && (prev == ' '
                        || prev == '{'
                        || prev == '['
                        || prev == ','
                        || prev == ':'
                        || prev == '-')
                {
                    quote = Some(c);
                } else if c == '#' && prev == ' ' {
                    break;
                }
            }
        }
        out.push(c);
        prev = c;
    }
    out.trim_end().to_owned()
}

fn parse_node(path: &str, lines: &mut Vec<Line>, idx: &mut usize, indent: usize) -> Result<Value> {
    let t = &lines[*idx].text;
    if t == "-" || t.starts_with("- ") {
        parse_seq(path, lines, idx, indent)
    } else if split_key(t).is_some() {
        parse_map(path, lines, idx, indent)
    } else {
        let v = parse_scalar_or_flow(path, &lines[*idx].text, lines[*idx].number)?;
        *idx += 1;
        Ok(v)
    }
}

fn parse_seq(path: &str, lines: &mut Vec<Line>, idx: &mut usize, indent: usize) -> Result<Value> {
    let mut out = Vec::new();
    while *idx < lines.len() && lines[*idx].indent == indent {
        let t = lines[*idx].text.clone();
        if !(t == "-" || t.starts_with("- ")) {
            break;
        }
        let rest = t[1..].trim_start().to_owned();
        let number = lines[*idx].number;
        if rest.is_empty() {
            *idx += 1;
            if *idx < lines.len() && lines[*idx].indent > indent {
                let ind = lines[*idx].indent;
                out.push(parse_node(path, lines, idx, ind)?);
            } else {
                out.push(Value::Null);
            }
        } else if rest == "-" || rest.starts_with("- ") || split_key(&rest).is_some() {
            let offset = t.len() - rest.len();
            lines[*idx].indent = indent + offset;
            lines[*idx].text = rest;
            let ind = lines[*idx].indent;
            out.push(parse_node(path, lines, idx, ind)?);
        } else {
            out.push(parse_scalar_or_flow(path, &rest, number)?);
            *idx += 1;
        }
    }
    Ok(Value::Array(out))
}

fn parse_map(path: &str, lines: &mut Vec<Line>, idx: &mut usize, indent: usize) -> Result<Value> {
    let mut map = Map::new();
    while *idx < lines.len() && lines[*idx].indent == indent {
        let t = lines[*idx].text.clone();
        let number = lines[*idx].number;
        let Some((key, rest)) = split_key(&t) else {
            return Err(DepsError::invalid(
                path,
                format!("line {number}: expected a mapping entry"),
            ));
        };
        let key = unquote_key(&key);
        *idx += 1;
        let value = if rest.is_empty() {
            if *idx < lines.len() && lines[*idx].indent > indent {
                let ind = lines[*idx].indent;
                parse_node(path, lines, idx, ind)?
            } else if *idx < lines.len()
                && lines[*idx].indent == indent
                && (lines[*idx].text == "-" || lines[*idx].text.starts_with("- "))
            {
                // A sequence at the same indent as its key.
                parse_seq(path, lines, idx, indent)?
            } else {
                Value::Null
            }
        } else {
            parse_scalar_or_flow(path, &rest, number)?
        };
        map.insert(key, value);
    }
    Ok(Value::Object(map))
}

/// Splits `key: value` at the first unquoted `: ` or trailing `:`.
fn split_key(t: &str) -> Option<(String, String)> {
    let b = t.as_bytes();
    let mut i = 0;
    if b.first().is_some_and(|c| *c == b'\'' || *c == b'"') {
        let q = b[0];
        i = 1;
        while i < b.len() {
            if b[i] == q {
                if q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                break;
            }
            if b[i] == b'\\' && q == b'"' {
                i += 1;
            }
            i += 1;
        }
        i += 1;
        let after = t.get(i..)?;
        let rest = after.strip_prefix(':')?;
        if rest.is_empty() || rest.starts_with(' ') {
            return Some((t[..i].to_owned(), rest.trim().to_owned()));
        }
        return None;
    }
    if t.starts_with(['{', '[']) {
        return None;
    }
    while i < b.len() {
        if b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' ') {
            return Some((t[..i].to_owned(), t[i + 1..].trim().to_owned()));
        }
        i += 1;
    }
    None
}

fn unquote_key(k: &str) -> String {
    match unquote(k) {
        Some(s) => s,
        None => k.to_owned(),
    }
}

fn unquote(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        return Some(s[1..s.len() - 1].replace("''", "'"));
    }
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        return serde_json::from_str::<String>(s)
            .ok()
            .or_else(|| Some(s[1..s.len() - 1].to_owned()));
    }
    None
}

fn parse_scalar_or_flow(path: &str, text: &str, number: usize) -> Result<Value> {
    let t = text.trim();
    if t.starts_with(['{', '[']) {
        let chars: Vec<char> = t.chars().collect();
        let mut pos = 0;
        let v = parse_flow(&chars, &mut pos).ok_or_else(|| {
            DepsError::invalid(path, format!("line {number}: malformed flow collection"))
        })?;
        return Ok(v);
    }
    Ok(scalar(t))
}

fn scalar(t: &str) -> Value {
    if let Some(s) = unquote(t) {
        return Value::String(s);
    }
    match t {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "~" | "null" | "" => Value::Null,
        _ => Value::String(t.to_owned()),
    }
}

fn skip_ws(c: &[char], pos: &mut usize) {
    while *pos < c.len() && c[*pos].is_whitespace() {
        *pos += 1;
    }
}

fn parse_flow(c: &[char], pos: &mut usize) -> Option<Value> {
    skip_ws(c, pos);
    match c.get(*pos)? {
        '{' => {
            *pos += 1;
            let mut map = Map::new();
            loop {
                skip_ws(c, pos);
                if *c.get(*pos)? == '}' {
                    *pos += 1;
                    return Some(Value::Object(map));
                }
                let key = flow_token(c, pos, true)?;
                skip_ws(c, pos);
                let value = if *c.get(*pos)? == ':' {
                    *pos += 1;
                    skip_ws(c, pos);
                    parse_flow(c, pos)?
                } else {
                    Value::Null
                };
                map.insert(unquote_key(&key), value);
                skip_ws(c, pos);
                if *c.get(*pos)? == ',' {
                    *pos += 1;
                }
            }
        }
        '[' => {
            *pos += 1;
            let mut out = Vec::new();
            loop {
                skip_ws(c, pos);
                if *c.get(*pos)? == ']' {
                    *pos += 1;
                    return Some(Value::Array(out));
                }
                out.push(parse_flow(c, pos)?);
                skip_ws(c, pos);
                if *c.get(*pos)? == ',' {
                    *pos += 1;
                }
            }
        }
        _ => {
            let tok = flow_token(c, pos, false)?;
            Some(scalar(&tok))
        }
    }
}

fn flow_token(c: &[char], pos: &mut usize, is_key: bool) -> Option<String> {
    skip_ws(c, pos);
    let start = *pos;
    let first = *c.get(*pos)?;
    if first == '\'' || first == '"' {
        *pos += 1;
        while *pos < c.len() {
            if c[*pos] == first {
                if first == '\'' && c.get(*pos + 1) == Some(&'\'') {
                    *pos += 2;
                    continue;
                }
                *pos += 1;
                return Some(c[start..*pos].iter().collect());
            }
            if c[*pos] == '\\' && first == '"' {
                *pos += 1;
            }
            *pos += 1;
        }
        return None;
    }
    while *pos < c.len() {
        let ch = c[*pos];
        if matches!(ch, ',' | '}' | ']')
            || (is_key && ch == ':' && c.get(*pos + 1).is_none_or(|n| n.is_whitespace()))
        {
            break;
        }
        *pos += 1;
    }
    Some(c[start..*pos].iter().collect::<String>().trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_maps_and_sequences() {
        let v = parse(
            "t",
            "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      left-pad:\n        specifier: ^1.3.0\n        version: 1.3.0\n  # comment\nlist:\n- a\n- b: 1\n  c: 2\n-   - x\n",
        )
        .unwrap();
        assert_eq!(v["lockfileVersion"], "9.0");
        assert_eq!(
            v["importers"]["."]["dependencies"]["left-pad"]["version"],
            "1.3.0"
        );
        assert_eq!(v["list"], json!(["a", {"b": "1", "c": "2"}, ["x"]]));
    }

    #[test]
    fn quoted_keys_flow_and_block_scalars() {
        let v = parse(
            "t",
            "packages:\n  '@babel/core@7.0.0':\n    resolution: {integrity: sha512-abc==, tarball: 'https://x/y.tgz'}\n    engines: {node: '>=6.9.0'}\n    cpu: [x64, arm64]\n    dev: false\n\"a@npm:^1, a@npm:^2\":\n  version: 2.0.0\ndescription: |\n  line one\n  line two\nafter: yes\n",
        )
        .unwrap();
        assert_eq!(
            v["packages"]["@babel/core@7.0.0"]["resolution"]["integrity"],
            "sha512-abc=="
        );
        assert_eq!(
            v["packages"]["@babel/core@7.0.0"]["engines"]["node"],
            ">=6.9.0"
        );
        assert_eq!(
            v["packages"]["@babel/core@7.0.0"]["cpu"],
            json!(["x64", "arm64"])
        );
        assert_eq!(v["packages"]["@babel/core@7.0.0"]["dev"], false);
        assert_eq!(v["a@npm:^1, a@npm:^2"]["version"], "2.0.0");
        assert_eq!(v["description"], "line one\nline two");
        assert_eq!(v["after"], "yes");
    }

    #[test]
    fn sequence_at_key_indent_and_comments() {
        let v = parse("t", "k:\n- a # trailing\n- 'b # not a comment'\nn: v\n").unwrap();
        assert_eq!(v["k"], json!(["a", "b # not a comment"]));
        assert_eq!(v["n"], "v");
    }

    #[test]
    fn empty_document_is_null() {
        assert_eq!(parse("t", "# only a comment\n").unwrap(), Value::Null);
    }
}
