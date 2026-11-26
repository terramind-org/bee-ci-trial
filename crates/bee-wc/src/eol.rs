//! Line endings by `.gitattributes` (SPEC §10.3, PRODUCT §4.1).
//!
//! The repository stores what the working copy commits. A path whose attributes make it text
//! is normalized on the way in: every CRLF becomes LF before the blob is hashed (status,
//! commit and diff agree). On the way out a text path with `eol=crlf` is written with CRLF;
//! every other path is written exactly as stored. Attributes:
//!
//! - `text` (and the old `crlf`): text; `-text`, `binary` (and the old `-crlf`): never
//!   converted; `text=auto`: text unless the content looks binary (a NUL byte in its first
//!   8,000 bytes);
//! - `eol=lf` / `eol=crlf`: the working-tree line ending of a text path; setting `eol` without
//!   `text` makes the path text, as git does.
//!
//! Patterns follow `.gitignore` rules (anchored when they contain `/`, otherwise matched
//! against the last component; no negation); a `.gitattributes` applies to its directory and
//! below, deeper files and later lines overriding earlier ones attribute by attribute. Paths
//! with no rule are never converted (no `core.autocrlf`: the server and every client agree).

use std::borrow::Cow;

use crate::ignore::glob_match;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Text {
    Set,
    Unset,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Eol {
    Lf,
    Crlf,
}

#[derive(Clone, Debug)]
struct Rule {
    base: Vec<u8>,
    glob: Vec<u8>,
    anchored: bool,
    text: Option<Text>,
    eol: Option<Eol>,
}

/// What happens to one path's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conversion {
    /// Stored and written as they are.
    None,
    /// CRLF becomes LF on commit; written with LF.
    Normalize,
    /// CRLF becomes LF on commit; written with CRLF.
    Crlf,
}

/// Parsed `.gitattributes` rules of a working tree.
#[derive(Clone, Debug, Default)]
pub struct Attributes {
    rules: Vec<Rule>,
}

/// True for a `.gitattributes` path.
pub fn is_attributes_file(path: &[u8]) -> bool {
    path.rsplit(|&b| b == b'/').next() == Some(b".gitattributes".as_slice())
}

/// The directory of a path (`""` for the root).
pub fn dir_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[..i],
        None => &[],
    }
}

impl Attributes {
    /// No rules.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no rule sets a line-ending attribute (nothing is ever converted).
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Adds the lines of the `.gitattributes` found in directory `base` (`""` for the root).
    /// Add parents before children.
    pub fn add_file(&mut self, base: &[u8], text: &[u8]) {
        for raw in text.split(|&b| b == b'\n') {
            let line = raw.strip_suffix(b"\r").unwrap_or(raw);
            let line = String::from_utf8_lossy(line);
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let Some(pattern) = words.next() else {
                continue;
            };
            let (mut text_attr, mut eol) = (None, None);
            for w in words {
                match w {
                    "text" | "crlf" => text_attr = Some(Text::Set),
                    "-text" | "binary" | "-crlf" => text_attr = Some(Text::Unset),
                    "text=auto" | "crlf=input" => text_attr = Some(Text::Auto),
                    "eol=lf" => eol = Some(Eol::Lf),
                    "eol=crlf" => eol = Some(Eol::Crlf),
                    _ => {}
                }
            }
            if text_attr.is_none() && eol.is_none() {
                continue;
            }
            let mut glob = pattern.as_bytes().to_vec();
            let anchored = glob.contains(&b'/');
            if glob.first() == Some(&b'/') {
                glob.remove(0);
            }
            if glob.is_empty() {
                continue;
            }
            self.rules.push(Rule {
                base: base.to_vec(),
                glob,
                anchored,
                text: text_attr,
                eol,
            });
        }
    }

    /// The conversion of `path`, whose (repository or working-tree) bytes are `content`.
    pub fn conversion(&self, path: &[u8], content: &[u8]) -> Conversion {
        let (mut text, mut eol) = (None, None);
        for r in &self.rules {
            let rel = if r.base.is_empty() {
                path
            } else if path.len() > r.base.len()
                && path.starts_with(&r.base)
                && path[r.base.len()] == b'/'
            {
                &path[r.base.len() + 1..]
            } else {
                continue;
            };
            let subject = if r.anchored {
                rel
            } else {
                rel.rsplit(|&b| b == b'/').next().unwrap_or(rel)
            };
            if glob_match(&r.glob, subject) {
                if r.text.is_some() {
                    text = r.text;
                }
                if r.eol.is_some() {
                    eol = r.eol;
                }
            }
        }
        let is_text = match text {
            Some(Text::Set) => true,
            Some(Text::Unset) => false,
            Some(Text::Auto) => !looks_binary(content),
            None => eol.is_some(),
        };
        if !is_text {
            return Conversion::None;
        }
        match eol {
            Some(Eol::Crlf) => Conversion::Crlf,
            _ => Conversion::Normalize,
        }
    }
}

/// A NUL byte in the first 8,000 bytes (git's heuristic).
pub fn looks_binary(content: &[u8]) -> bool {
    content.iter().take(8000).any(|&b| b == 0)
}

/// The bytes the repository stores for working-tree `bytes` (CRLF → LF when converted).
pub fn to_repo(conv: Conversion, bytes: Vec<u8>) -> Vec<u8> {
    if conv == Conversion::None || !bytes.windows(2).any(|w| w == b"\r\n") {
        return bytes;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// The bytes written to the working tree for stored `bytes` (lone LF → CRLF for `Crlf`).
pub fn to_worktree(conv: Conversion, bytes: &[u8]) -> Cow<'_, [u8]> {
    if conv != Conversion::Crlf || !bytes.contains(&b'\n') {
        return Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(b);
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_resolve_per_attribute_with_later_and_deeper_winning() {
        let mut a = Attributes::new();
        a.add_file(
            b"",
            b"* text=auto\n*.bat eol=crlf\n*.png binary\n# comment\n/docs/*.txt -text\n",
        );
        a.add_file(b"win", b"*.txt text eol=crlf\n");
        assert_eq!(
            a.conversion(b"src/a.rs", b"fn a() {}\n"),
            Conversion::Normalize
        );
        assert_eq!(
            a.conversion(b"src/a.rs", b"\0bin"),
            Conversion::None,
            "auto sees binary"
        );
        assert_eq!(
            a.conversion(b"run.bat", b"x\n"),
            Conversion::Crlf,
            "eol implies text"
        );
        assert_eq!(a.conversion(b"img/x.png", b"x\n"), Conversion::None);
        assert_eq!(
            a.conversion(b"docs/n.txt", b"x\n"),
            Conversion::None,
            "anchored"
        );
        assert_eq!(
            a.conversion(b"more/docs/n.txt", b"x\n"),
            Conversion::Normalize
        );
        assert_eq!(
            a.conversion(b"win/n.txt", b"x\n"),
            Conversion::Crlf,
            "deeper file"
        );
        assert_eq!(
            Attributes::new().conversion(b"a", b"x\r\n"),
            Conversion::None
        );
        assert!(is_attributes_file(b"a/b/.gitattributes"));
        assert!(!is_attributes_file(b"a/b.gitattributes"));
        assert_eq!(dir_of(b"a/b/.gitattributes"), b"a/b");
    }

    #[test]
    fn conversions_round_trip() {
        assert_eq!(
            to_repo(Conversion::Normalize, b"a\r\nb\nc\r\n".to_vec()),
            b"a\nb\nc\n"
        );
        assert_eq!(to_repo(Conversion::None, b"a\r\n".to_vec()), b"a\r\n");
        assert_eq!(
            &*to_worktree(Conversion::Crlf, b"a\nb\r\nc"),
            b"a\r\nb\r\nc"
        );
        assert_eq!(&*to_worktree(Conversion::Normalize, b"a\n"), b"a\n");
        let stored = b"one\ntwo\n".to_vec();
        let disk = to_worktree(Conversion::Crlf, &stored).into_owned();
        assert_eq!(to_repo(Conversion::Crlf, disk), stored);
    }
}
