//! `language_stats` (lazy, PRODUCT §4.4): bytes and files per language in a changeset's
//! tree, linguist-style.
//!
//! A leaf counts when it is a regular or executable file whose language is detectable:
//!
//! 1. `.gitattributes` `linguist-language=<Name>` names it (and makes it detectable);
//! 2. otherwise its file name (`Makefile`, `Dockerfile`, …), then its extension, then — for
//!    files with no extension — a `#!` line names a known language.
//!
//! Programming and markup languages count by default; data and prose languages (JSON, YAML,
//! Markdown, …) count only with `linguist-detectable`. Files marked (by attribute, or by the
//! default `vendor/`, `node_modules/`, `third_party/` and `docs/` rules) vendored, generated
//! or documentation do not count unless `linguist-detectable` is set; `-linguist-vendored`
//! and friends undo a default. `linguist-detectable=false` excludes a file.
//!
//! Derivation is forward from the first parent: the parent's totals minus the old versions
//! of the delta's paths plus the new ones. A delta that touches any `.gitattributes` (or a
//! parentless changeset) recomputes from the whole tree. Value, keyed by the changeset:
//!
//! ```text
//! {1: [[language name, files, bytes], ...]}   sorted by name
//! ```

use std::collections::BTreeMap;

use async_trait::async_trait;
use bee_core::{Mode as EntryMode, ObjectId, RepoPath, Result};
use bee_history::History;
use bee_history::codec::{self, Fields, as_array, as_uint, corrupt};
use bee_object::{DeltaOp, Value};

use crate::attributes::{AttrFile, AttrValue, Attributes};
use crate::types::{DerivedType, KeyKind, Mode};

/// The derived type name.
pub const LANGUAGE_STATS_TYPE: &str = "language_stats";
/// The derived type version.
pub const LANGUAGE_STATS_VERSION: u32 = 1;
const ATTRIBUTES_FILE: &[u8] = b".gitattributes";

/// Files and bytes of one language.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LanguageTotals {
    /// Files.
    pub files: u64,
    /// Bytes.
    pub bytes: u64,
}

/// Totals per language name.
pub type LanguageStats = BTreeMap<String, LanguageTotals>;

/// Encodes stats.
pub fn encode_stats(stats: &LanguageStats) -> Vec<u8> {
    codec::map([(
        1,
        Value::Array(
            stats
                .iter()
                .filter(|(_, t)| t.files > 0)
                .map(|(n, t)| {
                    Value::Array(vec![
                        Value::Text(n.clone()),
                        Value::Uint(t.files),
                        Value::Uint(t.bytes),
                    ])
                })
                .collect(),
        ),
    )])
    .encode()
}

/// Decodes stats.
pub fn decode_stats(bytes: &[u8]) -> Result<LanguageStats> {
    let what = LANGUAGE_STATS_TYPE;
    let f = Fields::decode(what, bytes)?;
    let mut out = LanguageStats::new();
    for row in f.opt_array(1)? {
        let r = as_array(what, &row)?;
        if r.len() != 3 {
            return Err(corrupt(what, "bad row"));
        }
        let Value::Text(name) = &r[0] else {
            return Err(corrupt(what, "bad name"));
        };
        out.insert(
            name.clone(),
            LanguageTotals {
                files: as_uint(what, &r[1])?,
                bytes: as_uint(what, &r[2])?,
            },
        );
    }
    Ok(out)
}

/// `(extension, language, counted by default)`.
const EXTENSIONS: &[(&str, &str, bool)] = &[
    ("rs", "Rust", true),
    ("go", "Go", true),
    ("py", "Python", true),
    ("pyi", "Python", true),
    ("rb", "Ruby", true),
    ("js", "JavaScript", true),
    ("mjs", "JavaScript", true),
    ("cjs", "JavaScript", true),
    ("jsx", "JavaScript", true),
    ("ts", "TypeScript", true),
    ("tsx", "TSX", true),
    ("java", "Java", true),
    ("kt", "Kotlin", true),
    ("swift", "Swift", true),
    ("c", "C", true),
    ("h", "C", true),
    ("cc", "C++", true),
    ("cpp", "C++", true),
    ("cxx", "C++", true),
    ("hpp", "C++", true),
    ("cs", "C#", true),
    ("php", "PHP", true),
    ("scala", "Scala", true),
    ("sh", "Shell", true),
    ("bash", "Shell", true),
    ("zsh", "Shell", true),
    ("pl", "Perl", true),
    ("lua", "Lua", true),
    ("ex", "Elixir", true),
    ("exs", "Elixir", true),
    ("erl", "Erlang", true),
    ("hs", "Haskell", true),
    ("ml", "OCaml", true),
    ("clj", "Clojure", true),
    ("dart", "Dart", true),
    ("r", "R", true),
    ("sql", "SQL", true),
    ("proto", "Protocol Buffer", true),
    ("html", "HTML", true),
    ("htm", "HTML", true),
    ("css", "CSS", true),
    ("scss", "SCSS", true),
    ("vue", "Vue", true),
    ("svelte", "Svelte", true),
    ("md", "Markdown", false),
    ("markdown", "Markdown", false),
    ("rst", "reStructuredText", false),
    ("txt", "Text", false),
    ("json", "JSON", false),
    ("yaml", "YAML", false),
    ("yml", "YAML", false),
    ("toml", "TOML", false),
    ("xml", "XML", false),
    ("csv", "CSV", false),
];

/// `(file name, language, counted by default)`.
const FILE_NAMES: &[(&str, &str, bool)] = &[
    ("Makefile", "Makefile", true),
    ("GNUmakefile", "Makefile", true),
    ("Dockerfile", "Dockerfile", true),
    ("CMakeLists.txt", "CMake", true),
    ("Rakefile", "Ruby", true),
    ("Gemfile", "Ruby", true),
    ("BUILD", "Starlark", true),
    ("BUILD.bazel", "Starlark", true),
];

/// `(interpreter, language)` for `#!` lines.
const INTERPRETERS: &[(&str, &str)] = &[
    ("python", "Python"),
    ("python3", "Python"),
    ("bash", "Shell"),
    ("sh", "Shell"),
    ("zsh", "Shell"),
    ("node", "JavaScript"),
    ("ruby", "Ruby"),
    ("perl", "Perl"),
];

/// Language of a path by name or extension: `(language, counted by default)`.
pub fn by_name(path: &RepoPath) -> Option<(&'static str, bool)> {
    let name = String::from_utf8_lossy(path.file_name()).into_owned();
    if let Some((_, l, c)) = FILE_NAMES.iter().find(|(n, _, _)| *n == name) {
        return Some((l, *c));
    }
    let (_, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(e, _, _)| *e == ext)
        .map(|(_, l, c)| (*l, *c))
}

/// True when a path has an extension (so its `#!` line is not consulted).
fn has_extension(path: &RepoPath) -> bool {
    let name = path.file_name();
    name.iter().skip(1).any(|b| *b == b'.')
}

/// Language of a `#!` line.
pub fn by_shebang(first_line: &[u8]) -> Option<&'static str> {
    let line = std::str::from_utf8(first_line).ok()?.strip_prefix("#!")?;
    let mut words = line.split_whitespace();
    let mut prog = words.next()?.rsplit('/').next()?;
    if prog == "env" {
        prog = words.find(|w| !w.starts_with('-'))?;
    }
    let prog = prog.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let prog = if prog.is_empty() { "python" } else { prog };
    INTERPRETERS
        .iter()
        .find(|(i, _)| *i == prog)
        .map(|(_, l)| *l)
}

fn default_excluded(path: &RepoPath) -> (bool, bool) {
    let comps: Vec<&[u8]> = path.components().collect();
    let dirs = &comps[..comps.len().saturating_sub(1)];
    let vendored = dirs
        .iter()
        .any(|c| matches!(*c, b"vendor" | b"node_modules" | b"third_party"));
    let documentation = dirs.first().is_some_and(|c| matches!(*c, b"docs" | b"doc"));
    (vendored, documentation)
}

/// Classifies one leaf; `first_line` is consulted only for extensionless files.
pub fn classify(
    path: &RepoPath,
    mode: EntryMode,
    attrs: &BTreeMap<String, AttrValue>,
    first_line: Option<&[u8]>,
) -> Option<String> {
    if !matches!(mode, EntryMode::File | EntryMode::Executable) {
        return None;
    }
    let detectable = attrs.get("linguist-detectable");
    if detectable.is_some_and(|v| !v.is_true()) {
        return None;
    }
    let forced_detectable = detectable.is_some_and(AttrValue::is_true);
    let (dv, dd) = default_excluded(path);
    let flag = |name: &str, default: bool| attrs.get(name).map_or(default, AttrValue::is_true);
    let excluded = flag("linguist-vendored", dv)
        || flag("linguist-generated", false)
        || flag("linguist-documentation", dd);
    if excluded && !forced_detectable {
        return None;
    }
    if let Some(AttrValue::Value(lang)) = attrs.get("linguist-language") {
        return Some(lang.clone());
    }
    let (lang, counted) = match by_name(path) {
        Some(x) => x,
        None if !has_extension(path) => (by_shebang(first_line?)?, true),
        None => return None,
    };
    (counted || forced_detectable).then(|| lang.to_string())
}

/// The `language_stats` type.
#[derive(Clone, Copy, Debug, Default)]
pub struct LanguageStatsType;

struct Scan<'a> {
    history: &'a History,
    root: ObjectId,
    attrs: Attributes,
    loaded_dirs: BTreeMap<Option<RepoPath>, ()>,
}

impl<'a> Scan<'a> {
    fn new(history: &'a History, root: ObjectId) -> Scan<'a> {
        Scan {
            history,
            root,
            attrs: Attributes::default(),
            loaded_dirs: BTreeMap::new(),
        }
    }

    async fn load_attr_file(&mut self, dir: Option<RepoPath>) -> Result<()> {
        if self.loaded_dirs.insert(dir.clone(), ()).is_some() {
            return Ok(());
        }
        let path = match &dir {
            Some(d) => d.join(ATTRIBUTES_FILE)?,
            None => RepoPath::new(ATTRIBUTES_FILE)?,
        };
        if let Some(e) = self.history.entry_at(&self.root, &path).await?
            && e.mode.is_blob()
        {
            let content = self.history.blob(&e.id).await?;
            self.attrs
                .insert(dir, AttrFile::parse(content.bytes_or_empty()));
        }
        Ok(())
    }

    /// The language of one leaf (loading the attribute files on its path).
    async fn language(
        &mut self,
        path: &RepoPath,
        mode: EntryMode,
        id: &ObjectId,
    ) -> Result<Option<String>> {
        self.load_attr_file(None).await?;
        for a in path.ancestors() {
            self.load_attr_file(Some(a)).await?;
        }
        let attrs = self.attrs.lookup(path);
        let needs_line = by_name(path).is_none()
            && !has_extension(path)
            && !matches!(attrs.get("linguist-language"), Some(AttrValue::Value(_)));
        let first = if needs_line && mode.is_blob() {
            let c = self.history.blob(id).await?;
            let b = c.bytes_or_empty();
            let end = b
                .iter()
                .position(|x| *x == b'\n')
                .unwrap_or(b.len())
                .min(256);
            Some(b[..end].to_vec())
        } else {
            None
        };
        Ok(classify(path, mode, &attrs, first.as_deref()))
    }
}

fn add(stats: &mut LanguageStats, lang: String, size: u64, sign: i8) {
    let t = stats.entry(lang).or_default();
    if sign > 0 {
        t.files += 1;
        t.bytes += size;
    } else {
        t.files = t.files.saturating_sub(1);
        t.bytes = t.bytes.saturating_sub(size);
    }
}

#[async_trait]
impl DerivedType for LanguageStatsType {
    fn name(&self) -> &'static str {
        LANGUAGE_STATS_TYPE
    }
    fn version(&self) -> u32 {
        LANGUAGE_STATS_VERSION
    }
    fn mode(&self) -> Mode {
        Mode::Lazy
    }
    fn key_kind(&self) -> KeyKind {
        KeyKind::Changeset
    }
    fn content_bearing(&self) -> bool {
        true
    }
    fn has_deps(&self) -> bool {
        true
    }
    async fn deps(&self, history: &History, key: &ObjectId) -> Result<Vec<ObjectId>> {
        let cs = history.changeset(key).await?;
        let touches_attrs = cs
            .delta()
            .iter()
            .any(|e| e.path.file_name() == ATTRIBUTES_FILE);
        Ok(match cs.first_parent() {
            Some(p) if !touches_attrs => vec![*p],
            _ => Vec::new(),
        })
    }
    async fn derive(
        &self,
        history: &History,
        key: &ObjectId,
        deps: &[(ObjectId, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        let cs = history.changeset(key).await?;
        let mut stats;
        match deps.first() {
            None => {
                stats = LanguageStats::new();
                let mut scan = Scan::new(history, *cs.root());
                for (p, e) in history.list_leaves(cs.root(), None).await? {
                    if let Some(l) = scan.language(&p, e.mode, &e.id).await? {
                        add(&mut stats, l, e.size, 1);
                    }
                }
            }
            Some((parent, value)) => {
                stats = decode_stats(value)?;
                let parent_root = *history.changeset(parent).await?.root();
                let mut old_scan = Scan::new(history, parent_root);
                let mut new_scan = Scan::new(history, *cs.root());
                for e in cs.delta() {
                    if let Some(o) = history.entry_at(&parent_root, &e.path).await?
                        && o.is_leaf()
                        && let Some(l) = old_scan.language(&e.path, o.mode, &o.id).await?
                    {
                        add(&mut stats, l, o.size, -1);
                    }
                    if let DeltaOp::Put { mode, id, size } = e.op
                        && let Some(l) = new_scan.language(&e.path, mode, &id).await?
                    {
                        add(&mut stats, l, size, 1);
                    }
                }
            }
        }
        Ok(encode_stats(&stats))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s.as_bytes()).unwrap()
    }

    #[test]
    fn detection_rules() {
        let none = BTreeMap::new();
        assert_eq!(
            classify(&p("src/a.rs"), EntryMode::File, &none, None).as_deref(),
            Some("Rust")
        );
        assert_eq!(
            classify(&p("Makefile"), EntryMode::File, &none, None).as_deref(),
            Some("Makefile")
        );
        assert_eq!(
            classify(&p("README.md"), EntryMode::File, &none, None),
            None
        );
        assert_eq!(
            classify(&p("vendor/x.js"), EntryMode::File, &none, None),
            None
        );
        assert_eq!(
            classify(&p("docs/x.py"), EntryMode::File, &none, None),
            None
        );
        assert_eq!(
            classify(&p("link.rs"), EntryMode::Symlink, &none, None),
            None
        );
        assert_eq!(
            classify(
                &p("bin/tool"),
                EntryMode::Executable,
                &none,
                Some(b"#!/usr/bin/env python3")
            )
            .as_deref(),
            Some("Python")
        );
        assert_eq!(
            classify(&p("bin/tool"), EntryMode::Executable, &none, Some(b"plain")),
            None
        );
        assert_eq!(by_shebang(b"#!/bin/sh -e"), Some("Shell"));
        assert_eq!(
            by_shebang(b"#!/usr/bin/env -S node --flag"),
            Some("JavaScript")
        );
        let mut attrs = BTreeMap::new();
        attrs.insert("linguist-detectable".to_string(), AttrValue::Set);
        assert_eq!(
            classify(&p("README.md"), EntryMode::File, &attrs, None).as_deref(),
            Some("Markdown")
        );
        let mut attrs = BTreeMap::new();
        attrs.insert("linguist-vendored".to_string(), AttrValue::Unset);
        assert_eq!(
            classify(&p("vendor/x.js"), EntryMode::File, &attrs, None).as_deref(),
            Some("JavaScript")
        );
        let mut attrs = BTreeMap::new();
        attrs.insert(
            "linguist-language".to_string(),
            AttrValue::Value("Rust".into()),
        );
        assert_eq!(
            classify(&p("x.weird"), EntryMode::File, &attrs, None).as_deref(),
            Some("Rust")
        );
        let mut attrs = BTreeMap::new();
        attrs.insert("linguist-generated".to_string(), AttrValue::Set);
        assert_eq!(classify(&p("gen.rs"), EntryMode::File, &attrs, None), None);
        let mut s = LanguageStats::new();
        add(&mut s, "Rust".into(), 10, 1);
        add(&mut s, "Go".into(), 3, 1);
        add(&mut s, "Go".into(), 3, -1);
        let back = decode_stats(&encode_stats(&s)).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(
            back["Rust"],
            LanguageTotals {
                files: 1,
                bytes: 10
            }
        );
    }
}
