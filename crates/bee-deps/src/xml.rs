//! A minimal XML reader for `pom.xml` and project files (`*.csproj`).
//!
//! It builds an element tree with attributes and text; it skips comments, processing
//! instructions, `DOCTYPE` and handles CDATA and the five predefined entities. Namespaces are
//! not resolved: element names keep their prefix, and [`Element::local_name`] strips it.

use crate::error::{DepsError, Result};

/// An XML element.
#[derive(Clone, Debug, Default)]
pub struct Element {
    /// The tag name including any prefix.
    pub name: String,
    /// Attributes in document order.
    pub attrs: Vec<(String, String)>,
    /// Child elements in document order.
    pub children: Vec<Element>,
    /// Concatenated text content directly inside the element, trimmed.
    pub text: String,
}

impl Element {
    /// The tag name without a namespace prefix.
    pub fn local_name(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }

    /// The first child with the given local name.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.local_name() == name)
    }

    /// All children with the given local name.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.local_name() == name)
    }

    /// The text of the first child with the given local name.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name)
            .map(|c| c.text.as_str())
            .filter(|t| !t.is_empty())
    }

    /// An attribute value by exact name (case-sensitive).
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// An attribute value by name, ignoring case (MSBuild is case-insensitive).
    pub fn attr_ci(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Visits this element and every descendant in document order.
    pub fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Element)) {
        f(self);
        for c in &self.children {
            c.walk(f);
        }
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let ent = &rest[1..end];
        match ent {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = ent
                    .strip_prefix("#x")
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()));
                match code.and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push_str(&rest[..=end]),
                }
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Parses an XML document and returns its root element.
pub fn parse(path: &str, text: &str) -> Result<Element> {
    let err = |m: &str| DepsError::invalid(path, format!("invalid XML: {m}"));
    let s = text.strip_prefix('\u{feff}').unwrap_or(text);
    let b = s.as_bytes();
    let mut i = 0;
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    while i < b.len() {
        if b[i] != b'<' {
            let end = s[i..].find('<').map_or(b.len(), |p| i + p);
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&unescape(&s[i..end]));
            }
            i = end;
            continue;
        }
        let rest = &s[i..];
        if rest.starts_with("<!--") {
            i += rest
                .find("-->")
                .ok_or_else(|| err("unterminated comment"))?
                + 3;
        } else if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").ok_or_else(|| err("unterminated CDATA"))?;
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&rest[9..end]);
            }
            i += end + 3;
        } else if rest.starts_with("<?") {
            i += rest
                .find("?>")
                .ok_or_else(|| err("unterminated processing instruction"))?
                + 2;
        } else if rest.starts_with("<!") {
            // DOCTYPE, possibly with an internal subset in brackets.
            let mut depth = 0usize;
            let mut j = i + 2;
            while j < b.len() {
                match b[j] {
                    b'[' => depth += 1,
                    b']' => depth = depth.saturating_sub(1),
                    b'>' if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            i = j + 1;
        } else if rest.starts_with("</") {
            let end = rest.find('>').ok_or_else(|| err("unterminated end tag"))?;
            let name = rest[2..end].trim();
            let mut el = stack.pop().ok_or_else(|| err("unexpected end tag"))?;
            if el.name != name {
                return Err(err(&format!(
                    "end tag </{name}> does not match <{}>",
                    el.name
                )));
            }
            el.text = el.text.trim().to_owned();
            match stack.last_mut() {
                Some(parent) => parent.children.push(el),
                None => root = Some(el),
            }
            i += end + 1;
        } else {
            let end = tag_end(b, i).ok_or_else(|| err("unterminated tag"))?;
            let inner = &s[i + 1..end];
            let self_closing = inner.ends_with('/');
            let inner = inner.strip_suffix('/').unwrap_or(inner);
            let (name, attrs) = parse_tag(inner).ok_or_else(|| err("malformed tag"))?;
            let el = Element {
                name,
                attrs,
                children: Vec::new(),
                text: String::new(),
            };
            if self_closing {
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => root = Some(el),
                }
            } else {
                stack.push(el);
            }
            i = end + 1;
        }
    }
    if !stack.is_empty() {
        return Err(err("unclosed element"));
    }
    root.ok_or_else(|| err("no root element"))
}

fn tag_end(b: &[u8], start: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (off, &c) in b[start..].iter().enumerate() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'>' => return Some(start + off),
            None => {}
        }
    }
    None
}

fn parse_tag(inner: &str) -> Option<(String, Vec<(String, String)>)> {
    let inner = inner.trim();
    let name_end = inner
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inner.len());
    let name = &inner[..name_end];
    if name.is_empty() {
        return None;
    }
    let mut attrs = Vec::new();
    let mut rest = inner[name_end..].trim_start();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let key = rest[..eq].trim().to_owned();
        let after = rest[eq + 1..].trim_start();
        let q = after.chars().next()?;
        if q != '"' && q != '\'' {
            return None;
        }
        let close = after[1..].find(q)? + 1;
        attrs.push((key, unescape(&after[1..close])));
        rest = after[close + 1..].trim_start();
    }
    Some((name.to_owned(), attrs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_tree() {
        let e = parse(
            "pom.xml",
            "<?xml version=\"1.0\"?><!DOCTYPE x [<!ENTITY a \"b\">]>\n<project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n<!-- c -->\n<groupId>g</groupId><dependencies><dependency><artifactId>a&amp;b</artifactId><version><![CDATA[1.0]]></version></dependency><x a='1' b=\"2\"/></dependencies></project>",
        )
        .unwrap();
        assert_eq!(e.child_text("groupId"), Some("g"));
        let deps = e.child("dependencies").unwrap();
        let d = deps.child("dependency").unwrap();
        assert_eq!(d.child_text("artifactId"), Some("a&b"));
        assert_eq!(d.child_text("version"), Some("1.0"));
        assert_eq!(deps.child("x").unwrap().attr("b"), Some("2"));
    }

    #[test]
    fn rejects_mismatched_tags() {
        assert!(parse("p", "<a><b></a></b>").is_err());
        assert!(parse("p", "<a>").is_err());
        assert!(parse("p", "").is_err());
    }
}
