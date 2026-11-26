//! Update planner: turns vulnerability [`Finding`]s into minimal version bumps.
//!
//! The plan is data an update bot agent can turn into a change request: [`TextEdit`]s (file,
//! byte span, old text, new text) for manifests, plus lockfile refresh commands. Implemented for
//! Cargo (`Cargo.toml`), npm (`package.json`), pip (`requirements*.txt`) and Go (`go.mod`);
//! findings in other ecosystems are reported in [`UpdatePlan::unfixable`]. See
//! `docs/spec/security-deps.md` for the limits.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use toml::de::{DeTable, DeValue};

use crate::cvss::Severity;
use crate::error::{DepsError, Result};
use crate::matching::Finding;
use crate::model::{DependencyGraph, Ecosystem, Relationship};
use crate::osv::{AdvisorySource, Affected};
use crate::parse::{SourceFile, dir_of, within};
use crate::version;

/// A text replacement in one file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextEdit {
    /// Repo-relative path.
    pub path: String,
    /// Byte offset where `old` starts.
    pub start: usize,
    /// Byte offset where `old` ends.
    pub end: usize,
    /// The text being replaced; the edit only applies when the file still has it there.
    pub old: String,
    /// The replacement.
    pub new: String,
}

/// One package bump.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedUpdate {
    /// The ecosystem.
    pub ecosystem: Ecosystem,
    /// The package name.
    pub package: String,
    /// The version in use.
    pub from: String,
    /// The version to move to: the lowest fixed version that no known advisory still affects.
    pub to: String,
    /// Advisories fixed by the bump, sorted.
    pub advisories: Vec<String>,
    /// The highest severity among them.
    pub severity: Severity,
    /// Direct or indirect.
    pub relationship: Relationship,
    /// The lockfile or manifest that declared the finding.
    pub source_path: String,
    /// Manifest edits, sorted by path and offset.
    pub edits: Vec<TextEdit>,
    /// Commands that refresh the lockfile or tidy modules after the edits, in order.
    pub commands: Vec<String>,
    /// Caveats for the reviewer.
    pub notes: Vec<String>,
}

/// All updates of one ecosystem, the unit of a grouped update change request.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateGroup {
    /// The ecosystem.
    pub ecosystem: Ecosystem,
    /// Suggested change request title.
    pub title: String,
    /// The updates, sorted by package.
    pub updates: Vec<PlannedUpdate>,
}

impl UpdateGroup {
    /// Every edit of the group, de-duplicated and sorted by path and offset.
    pub fn edits(&self) -> Vec<TextEdit> {
        let set: BTreeSet<TextEdit> = self
            .updates
            .iter()
            .flat_map(|u| u.edits.iter().cloned())
            .collect();
        let mut v: Vec<TextEdit> = set.into_iter().collect();
        v.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
        v
    }

    /// Every command of the group, de-duplicated in first-seen order.
    pub fn commands(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in self.updates.iter().flat_map(|u| u.commands.iter()) {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
        out
    }
}

/// A finding the planner could not turn into a bump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unfixable {
    /// The package URL.
    pub purl: String,
    /// The advisories involved, sorted.
    pub advisories: Vec<String>,
    /// Why there is no automatic fix.
    pub reason: String,
}

/// The result of [`plan_security_updates`].
#[derive(Clone, Debug, PartialEq, Default)]
pub struct UpdatePlan {
    /// One group per ecosystem, ordered by ecosystem.
    pub groups: Vec<UpdateGroup>,
    /// Findings without an automatic fix.
    pub unfixable: Vec<Unfixable>,
}

impl UpdatePlan {
    /// Every edit across groups.
    pub fn edits(&self) -> Vec<TextEdit> {
        self.groups.iter().flat_map(UpdateGroup::edits).collect()
    }
}

/// Applies edits to one file's content.
///
/// Fails with `INVALID_ARGUMENT` when an edit's `old` text is not at its span (the file changed)
/// or two edits overlap.
pub fn apply_edits(content: &str, edits: &[TextEdit]) -> Result<String> {
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| (e.start, e.end));
    sorted.dedup();
    for w in sorted.windows(2) {
        if w[0].end > w[1].start {
            return Err(DepsError::invalid(&w[0].path, "overlapping edits"));
        }
    }
    let mut out = content.to_owned();
    for e in sorted.iter().rev() {
        if content.get(e.start..e.end) != Some(e.old.as_str()) {
            return Err(DepsError::invalid(
                &e.path,
                format!("text at {}..{} is not {:?}", e.start, e.end, e.old),
            ));
        }
        out.replace_range(e.start..e.end, &e.new);
    }
    Ok(out)
}

/// Applies a plan's edits to a set of files, returning the changed files.
pub fn apply_plan(files: &[SourceFile], plan: &UpdatePlan) -> Result<Vec<SourceFile>> {
    let mut by_path: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();
    for e in plan.edits() {
        by_path.entry(e.path.clone()).or_default().push(e);
    }
    let mut out = Vec::new();
    for (path, edits) in by_path {
        let f = files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| DepsError::invalid(&path, "file to edit is not in the revision"))?;
        out.push(SourceFile {
            path,
            content: apply_edits(&f.content, &edits)?,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Target version selection
// ---------------------------------------------------------------------------------------------

fn normalize_target(eco: Ecosystem, v: &str) -> String {
    if eco == Ecosystem::Go && !v.starts_with('v') {
        format!("v{v}")
    } else {
        v.to_owned()
    }
}

fn still_vulnerable(
    source: &dyn AdvisorySource,
    eco: Ecosystem,
    name: &str,
    version: &str,
) -> bool {
    source
        .candidates(eco, name)
        .iter()
        .filter(|a| !a.is_withdrawn())
        .any(|a| {
            a.affected.iter().any(|aff: &Affected| {
                aff.package.as_ref().is_some_and(|p| {
                    Ecosystem::from_osv_name(&p.ecosystem) == Some(eco)
                        && eco.normalize_name(&p.name) == eco.normalize_name(name)
                }) && crate::matching::is_affected(eco, version, aff, None)
            })
        })
}

// ---------------------------------------------------------------------------------------------
// Requirement rewriting
// ---------------------------------------------------------------------------------------------

/// Rewrites a simple single-comparator npm or Cargo requirement so its lower bound is `target`.
fn bump_simple_requirement(
    eco: Ecosystem,
    old: &str,
    target: &str,
) -> std::result::Result<String, String> {
    let t = old.trim();
    if t.is_empty()
        || t.contains([' ', ',', '|', '*', 'x', 'X']) && !t.starts_with('=')
        || t.contains("||")
    {
        return Err(format!(
            "requirement {old:?} is not a single version comparator"
        ));
    }
    let (op, rest) = ["^", "~", ">=", "=", ">", "<=", "<"]
        .iter()
        .find_map(|o| t.strip_prefix(o).map(|r| (*o, r)))
        .unwrap_or(("", t));
    if matches!(op, "<" | "<=") {
        return Err(format!(
            "requirement {old:?} caps the version below the fix"
        ));
    }
    if !version::is_valid(eco, rest.trim()) || rest.contains(['/', ':']) {
        return Err(format!("requirement {old:?} is not a version range"));
    }
    let op = if op == ">" { ">=" } else { op };
    let new = format!("{op}{target}");
    if version::satisfies(eco, target, &new) == Some(true) {
        Ok(new)
    } else {
        Err(format!(
            "rewritten requirement {new:?} would not allow {target}"
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Cargo
// ---------------------------------------------------------------------------------------------

/// Finds the declarations of one package in one manifest: `(path, text, package name)`.
type DeclScanner = fn(&str, &str, &str) -> Vec<Decl>;

/// A requirement string found in a manifest.
#[derive(Clone, Debug)]
struct Decl {
    path: String,
    requirement: String,
    /// Span of the requirement text (inside the quotes).
    span: Range<usize>,
}

fn string_decl(path: &str, text: &str, req: &str, span: Range<usize>) -> Option<Decl> {
    let lit = text.get(span.clone())?;
    let q = lit.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    if lit.len() < 2 || !lit.ends_with(q) || lit.starts_with("\"\"\"") || lit.starts_with("'''") {
        return None;
    }
    Some(Decl {
        path: path.to_owned(),
        requirement: req.to_owned(),
        span: span.start + 1..span.end - 1,
    })
}

fn cargo_table_decls(path: &str, text: &str, table: &DeTable<'_>, name: &str, out: &mut Vec<Decl>) {
    for (k, v) in table.iter() {
        let key: &str = k.get_ref();
        match v.get_ref() {
            DeValue::String(s) if key == name => {
                out.extend(string_decl(path, text, s, v.span()));
            }
            DeValue::Table(t) => {
                let mut real = key;
                for (tk, tv) in t.iter() {
                    if let (true, DeValue::String(p)) = (
                        <str as PartialEq>::eq(tk.get_ref(), "package"),
                        tv.get_ref(),
                    ) {
                        real = p;
                    }
                }
                if real != name {
                    continue;
                }
                for (tk, tv) in t.iter() {
                    if let (true, DeValue::String(s)) = (
                        <str as PartialEq>::eq(tk.get_ref(), "version"),
                        tv.get_ref(),
                    ) {
                        out.extend(string_decl(path, text, s, tv.span()));
                    }
                }
            }
            _ => {}
        }
    }
}

fn cargo_dep_sections(path: &str, text: &str, t: &DeTable<'_>, name: &str, out: &mut Vec<Decl>) {
    for (k, v) in t.iter() {
        if matches!(
            k.get_ref().as_ref(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) && let DeValue::Table(deps) = v.get_ref()
        {
            cargo_table_decls(path, text, deps, name, out);
        }
    }
}

fn cargo_decls(path: &str, text: &str, name: &str) -> Vec<Decl> {
    let Ok(doc) = DeTable::parse(text) else {
        return Vec::new();
    };
    let root = doc.get_ref();
    let mut out = Vec::new();
    cargo_dep_sections(path, text, root, name, &mut out);
    for (k, v) in root.iter() {
        match (k.get_ref().as_ref(), v.get_ref()) {
            ("target", DeValue::Table(targets)) => {
                for (_, tv) in targets.iter() {
                    if let DeValue::Table(t) = tv.get_ref() {
                        cargo_dep_sections(path, text, t, name, &mut out);
                    }
                }
            }
            ("workspace", DeValue::Table(ws)) => {
                for (wk, wv) in ws.iter() {
                    if wk.get_ref().as_ref() == "dependencies"
                        && let DeValue::Table(deps) = wv.get_ref()
                    {
                        cargo_table_decls(path, text, deps, name, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// npm (package.json)
// ---------------------------------------------------------------------------------------------

type JsonSpan = (Vec<String>, Range<usize>);

fn json_skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && b[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn json_string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn json_scan(
    text: &str,
    i: &mut usize,
    path: &mut Vec<String>,
    out: &mut Vec<JsonSpan>,
) -> Option<()> {
    let b = text.as_bytes();
    json_skip_ws(b, i);
    match *b.get(*i)? {
        b'{' => {
            *i += 1;
            loop {
                json_skip_ws(b, i);
                match *b.get(*i)? {
                    b'}' => {
                        *i += 1;
                        return Some(());
                    }
                    b',' => *i += 1,
                    b'"' => {
                        let end = json_string_end(b, *i)?;
                        let key: String = serde_json::from_str(&text[*i..=end]).ok()?;
                        *i = end + 1;
                        json_skip_ws(b, i);
                        if *b.get(*i)? != b':' {
                            return None;
                        }
                        *i += 1;
                        path.push(key);
                        json_scan(text, i, path, out)?;
                        path.pop();
                    }
                    _ => return None,
                }
            }
        }
        b'[' => {
            *i += 1;
            loop {
                json_skip_ws(b, i);
                match *b.get(*i)? {
                    b']' => {
                        *i += 1;
                        return Some(());
                    }
                    b',' => *i += 1,
                    _ => {
                        path.push("[]".to_owned());
                        json_scan(text, i, path, out)?;
                        path.pop();
                    }
                }
            }
        }
        b'"' => {
            let end = json_string_end(b, *i)?;
            out.push((path.clone(), *i + 1..end));
            *i = end + 1;
            Some(())
        }
        _ => {
            while *i < b.len()
                && !matches!(b[*i], b',' | b'}' | b']')
                && !b[*i].is_ascii_whitespace()
            {
                *i += 1;
            }
            Some(())
        }
    }
}

fn npm_decls(path: &str, text: &str, name: &str) -> Vec<Decl> {
    let mut spans = Vec::new();
    let mut i = 0;
    if json_scan(text, &mut i, &mut Vec::new(), &mut spans).is_none() {
        return Vec::new();
    }
    spans
        .into_iter()
        .filter(|(p, _)| {
            p.len() == 2
                && matches!(
                    p[0].as_str(),
                    "dependencies" | "devDependencies" | "optionalDependencies"
                )
                && p[1] == name
        })
        .map(|(_, r)| Decl {
            path: path.to_owned(),
            requirement: text[r.clone()].to_owned(),
            span: r,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// pip requirements
// ---------------------------------------------------------------------------------------------

/// Edits the version of the first eligible clause of the requirement for `name`, if any.
fn pip_edit(
    path: &str,
    text: &str,
    name: &str,
    target: &str,
) -> std::result::Result<Option<TextEdit>, String> {
    let want = Ecosystem::PyPI.normalize_name(name);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let body = line.trim_end_matches(['\n', '\r']);
        let lead = body.len() - body.trim_start().len();
        let rest = &body[lead..];
        if rest.starts_with(['#', '-']) {
            continue;
        }
        let name_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
            .unwrap_or(rest.len());
        if Ecosystem::PyPI.normalize_name(&rest[..name_len]) != want {
            continue;
        }
        let mut pos = lead + name_len;
        let after = &body[pos..];
        let skip = after.len() - after.trim_start().len();
        pos += skip;
        if body[pos..].starts_with('[') {
            pos += body[pos..].find(']').map_or(0, |i| i + 1);
        }
        let spec_text = &body[pos..];
        let spec_len = spec_text.find([';', '\\', '#']).unwrap_or(spec_text.len());
        let spec_len = spec_text[..spec_len].find(" --").unwrap_or(spec_len);
        let spec = &spec_text[..spec_len];
        let spec_trim = spec.trim_end();
        // Walk comma-separated clauses, tracking their offsets.
        let mut cursor = 0;
        for clause in spec_trim.split(',') {
            let c_start = cursor;
            cursor += clause.len() + 1;
            let lead_ws = clause.len() - clause.trim_start().len();
            let c = clause.trim();
            let Some((op, ver)) = ["===", "~=", "==", ">="]
                .iter()
                .find_map(|o| c.strip_prefix(o).map(|v| (*o, v)))
            else {
                continue;
            };
            let ver_trim = ver.trim_start();
            if ver_trim.ends_with(".*") {
                return Err(format!("{c:?} is a wildcard pin that excludes {target}"));
            }
            let v_off = pos + c_start + lead_ws + op.len() + (ver.len() - ver_trim.len());
            let new_spec = format!(
                "{}{}{}",
                &spec_trim[..v_off - pos],
                target,
                &spec_trim[v_off - pos + ver_trim.len()..]
            );
            let admitted = version::satisfies(Ecosystem::PyPI, target, new_spec.trim());
            if admitted != Some(true) {
                return Err(format!(
                    "{:?} would not allow {target} after the bump",
                    new_spec.trim()
                ));
            }
            let abs = line_start + v_off;
            return Ok(Some(TextEdit {
                path: path.to_owned(),
                start: abs,
                end: abs + ver_trim.len(),
                old: ver_trim.to_owned(),
                new: target.to_owned(),
            }));
        }
        return Err(format!(
            "requirement for {name} has no pin or lower bound to raise"
        ));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------------------------

fn go_edit(
    path: &str,
    text: &str,
    module: &str,
    installed: &str,
    target: &str,
) -> Option<TextEdit> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let code_end = line.find("//").unwrap_or(line.len());
        let code = &line[..code_end];
        let mut toks: Vec<(usize, &str)> = Vec::new();
        let mut i = 0;
        let bytes = code.as_bytes();
        while i < bytes.len() {
            if bytes[i].is_ascii_whitespace() {
                i += 1;
                continue;
            }
            let s = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            toks.push((s, &code[s..i]));
        }
        if toks.first().is_some_and(|(_, t)| *t == "require") {
            toks.remove(0);
        } else if toks
            .first()
            .is_some_and(|(_, t)| matches!(*t, "replace" | "exclude" | "retract" | "module" | "go"))
        {
            continue;
        }
        if toks.len() == 2 && toks[0].1 == module && toks[1].1 == installed {
            let (s, t) = toks[1];
            return Some(TextEdit {
                path: path.to_owned(),
                start: line_start + s,
                end: line_start + s + t.len(),
                old: t.to_owned(),
                new: target.to_owned(),
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------------------------

struct Occurrence<'a> {
    findings: Vec<&'a Finding>,
}

fn file_content<'a>(files: &'a [SourceFile], path: &str) -> Option<&'a str> {
    files
        .iter()
        .find(|f| f.path == path)
        .map(|f| f.content.as_str())
}

fn manifests_under<'a>(files: &'a [SourceFile], file_name: &str, dir: &str) -> Vec<&'a SourceFile> {
    let mut v: Vec<&SourceFile> = files
        .iter()
        .filter(|f| f.path.rsplit('/').next() == Some(file_name) && within(dir, &f.path))
        .collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

fn lock_command(
    eco: Ecosystem,
    source_path: &str,
    files: &[SourceFile],
    name: &str,
    from: &str,
    to: &str,
) -> Option<String> {
    let base = source_path.rsplit('/').next().unwrap_or(source_path);
    Some(match (eco, base) {
        (Ecosystem::Cargo, "Cargo.lock") => format!("cargo update -p {name}@{from} --precise {to}"),
        (Ecosystem::Npm, "package-lock.json" | "npm-shrinkwrap.json") => {
            format!("npm install {name}@{to} --package-lock-only")
        }
        (Ecosystem::Npm, "pnpm-lock.yaml") => format!("pnpm update {name}@{to}"),
        (Ecosystem::Npm, "yarn.lock") => {
            if file_content(files, source_path).is_some_and(|c| c.contains("__metadata:")) {
                format!("yarn up {name}@{to}")
            } else {
                format!("yarn upgrade {name}@{to}")
            }
        }
        (Ecosystem::PyPI, "poetry.lock") => format!("poetry update {name}"),
        (Ecosystem::PyPI, "uv.lock") => format!("uv lock --upgrade-package {name}=={to}"),
        (Ecosystem::PyPI, "Pipfile.lock") => format!("pipenv update {name}"),
        (Ecosystem::Go, _) => "go mod tidy".to_owned(),
        _ => return None,
    })
}

fn group_title(eco: Ecosystem, updates: &[PlannedUpdate]) -> String {
    let names: Vec<&str> = updates.iter().map(|u| u.package.as_str()).take(3).collect();
    let more = updates.len().saturating_sub(3);
    let list = names.join(", ");
    if more > 0 {
        format!(
            "Security: bump {} dependencies ({list} and {more} more)",
            eco.proto_name()
        )
    } else {
        format!("Security: bump {} {list}", eco.proto_name())
    }
}

/// Plans minimal security updates for `findings`.
///
/// For each package occurrence the target is the lowest fixed version that fixes every finding
/// and that no known advisory in `source` still affects; pre-releases are skipped unless the
/// installed version is one. A direct dependency whose declared range already allows the target
/// needs only a lockfile refresh (a command); otherwise the manifest requirement is edited,
/// keeping its operator. Complex requirements (`||`, hyphen ranges, wildcards, caps below the
/// fix) are reported as unfixable rather than guessed.
pub fn plan_security_updates(
    files: &[SourceFile],
    graph: &DependencyGraph,
    findings: &[Finding],
    source: &dyn AdvisorySource,
) -> UpdatePlan {
    let mut occurrences: BTreeMap<(String, String), Occurrence<'_>> = BTreeMap::new();
    for f in findings {
        occurrences
            .entry((f.manifest_path.clone(), f.purl.clone()))
            .or_insert_with(|| Occurrence {
                findings: Vec::new(),
            })
            .findings
            .push(f);
    }
    let mut plan = UpdatePlan::default();
    let mut by_eco: BTreeMap<Ecosystem, Vec<PlannedUpdate>> = BTreeMap::new();
    for ((source_path, purl), occ) in &occurrences {
        let first = occ.findings[0];
        let eco = first.ecosystem;
        let mut advisories: Vec<String> =
            occ.findings.iter().map(|f| f.advisory_id.clone()).collect();
        advisories.sort();
        advisories.dedup();
        let unfixable = |reason: String| Unfixable {
            purl: purl.clone(),
            advisories: advisories.clone(),
            reason,
        };
        if !matches!(
            eco,
            Ecosystem::Cargo | Ecosystem::Npm | Ecosystem::PyPI | Ecosystem::Go
        ) {
            plan.unfixable.push(unfixable(format!(
                "the update planner does not support {} yet",
                eco.proto_name()
            )));
            continue;
        }
        // Lowest version that fixes every finding.
        let mut lower: Option<String> = None;
        for f in &occ.findings {
            match f.fixed_versions.first() {
                None => {
                    lower = None;
                    break;
                }
                Some(v) => {
                    let v = normalize_target(eco, v);
                    if lower
                        .as_ref()
                        .is_none_or(|l| version::compare(eco, &v, l).is_gt())
                    {
                        lower = Some(v);
                    }
                }
            }
        }
        let Some(lower) = lower else {
            plan.unfixable.push(unfixable(
                "an advisory has no published fixed version".to_owned(),
            ));
            continue;
        };
        let mut candidates: Vec<String> = occ
            .findings
            .iter()
            .flat_map(|f| f.fixed_versions.iter())
            .map(|v| normalize_target(eco, v))
            .filter(|v| version::compare(eco, v, &lower).is_ge())
            .filter(|v| {
                version::is_prerelease(eco, &first.installed_version)
                    || !version::is_prerelease(eco, v)
            })
            .collect();
        candidates.sort_by(|a, b| version::compare(eco, a, b));
        candidates.dedup();
        let Some(target) = candidates
            .into_iter()
            .find(|c| !still_vulnerable(source, eco, &first.package, c))
        else {
            plan.unfixable.push(unfixable(
                "every known fixed version is affected by another advisory".to_owned(),
            ));
            continue;
        };
        let installed = first.installed_version.clone();
        let severity = occ
            .findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or(Severity::None);
        let has_lock = graph
            .manifests()
            .iter()
            .any(|m| m.path == *source_path && m.is_lockfile);
        let mut update = PlannedUpdate {
            ecosystem: eco,
            package: first.package.clone(),
            from: installed.clone(),
            to: target.clone(),
            advisories: advisories.clone(),
            severity,
            relationship: first.relationship,
            source_path: source_path.clone(),
            edits: Vec::new(),
            commands: Vec::new(),
            notes: Vec::new(),
        };
        let lock_dir = dir_of(source_path).to_owned();
        let mut failure: Option<String> = None;
        match eco {
            Ecosystem::Go => match file_content(files, source_path)
                .and_then(|t| go_edit(source_path, t, &first.package, &installed, &target))
            {
                Some(e) => {
                    update.edits.push(e);
                    update.commands.push("go mod tidy".to_owned());
                }
                None => {
                    failure = Some(format!(
                        "no `require {} {installed}` line found in {source_path}",
                        first.package
                    ))
                }
            },
            Ecosystem::PyPI => {
                if source_path
                    .rsplit('/')
                    .next()
                    .is_some_and(|b| b.ends_with(".txt"))
                {
                    match file_content(files, source_path)
                        .map(|t| pip_edit(source_path, t, &first.package, &target))
                    {
                        Some(Ok(Some(e))) => update.edits.push(e),
                        Some(Ok(None)) | None => {
                            failure = Some(format!("{} not found in {source_path}", first.package))
                        }
                        Some(Err(why)) => failure = Some(why),
                    }
                } else if let Some(c) =
                    lock_command(eco, source_path, files, &first.package, &installed, &target)
                {
                    update.commands.push(c);
                    update.notes.push(
                        "lockfile refresh only; the manifest requirement is not edited".to_owned(),
                    );
                } else {
                    failure = Some("no update path for this file".to_owned());
                }
            }
            Ecosystem::Cargo | Ecosystem::Npm => {
                let (file_name, scan): (&str, DeclScanner) = if eco == Ecosystem::Cargo {
                    ("Cargo.toml", cargo_decls)
                } else {
                    ("package.json", npm_decls)
                };
                let dir = if has_lock || !source_path.ends_with(file_name) {
                    lock_dir.as_str()
                } else {
                    dir_of(source_path)
                };
                let mut decls: Vec<Decl> = Vec::new();
                if source_path.ends_with(file_name) {
                    if let Some(t) = file_content(files, source_path) {
                        decls.extend(scan(source_path, t, &first.package));
                    }
                } else {
                    for m in manifests_under(files, file_name, dir) {
                        decls.extend(scan(&m.path, &m.content, &first.package));
                    }
                }
                decls.sort_by(|a, b| (&a.path, a.span.start).cmp(&(&b.path, b.span.start)));
                if decls.is_empty() && first.relationship == Relationship::Direct {
                    update.notes.push(
                        "the declaring manifest was not found; only the lockfile can change"
                            .to_owned(),
                    );
                }
                if first.relationship == Relationship::Indirect {
                    update.notes.push(format!(
                        "transitive dependency (via {}); the lockfile refresh needs the parents' ranges to allow {target}",
                        first.dependency_path.iter().rev().skip(1).take(1).cloned().collect::<Vec<_>>().join("")
                    ));
                } else {
                    let flavor_eco = eco;
                    for d in &decls {
                        let admits =
                            version::satisfies(flavor_eco, &target, &d.requirement) == Some(true);
                        let lower_ok = version::min_version(flavor_eco, &d.requirement)
                            .is_some_and(|m| version::compare(flavor_eco, &m, &target).is_ge());
                        if has_lock && admits || lower_ok {
                            continue;
                        }
                        match bump_simple_requirement(flavor_eco, &d.requirement, &target) {
                            Ok(new) => update.edits.push(TextEdit {
                                path: d.path.clone(),
                                start: d.span.start,
                                end: d.span.end,
                                old: d.requirement.clone(),
                                new,
                            }),
                            Err(why) => {
                                failure = Some(why);
                                break;
                            }
                        }
                    }
                }
                if failure.is_none()
                    && has_lock
                    && let Some(c) =
                        lock_command(eco, source_path, files, &first.package, &installed, &target)
                {
                    update.commands.push(c);
                }
                if failure.is_none() && update.edits.is_empty() && update.commands.is_empty() {
                    failure = Some("nothing to change: no lockfile command and the declared range already requires the fix".to_owned());
                }
            }
            _ => {}
        }
        if let Some(why) = failure {
            plan.unfixable.push(unfixable(why));
            continue;
        }
        update.edits.sort();
        by_eco.entry(eco).or_default().push(update);
    }
    for (eco, mut updates) in by_eco {
        updates.sort_by(|a, b| (&a.package, &a.source_path).cmp(&(&b.package, &b.source_path)));
        plan.groups.push(UpdateGroup {
            ecosystem: eco,
            title: group_title(eco, &updates),
            updates,
        });
    }
    plan.unfixable.sort_by(|a, b| a.purl.cmp(&b.purl));
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_requirement_rewrites() {
        let n = Ecosystem::Npm;
        assert_eq!(
            bump_simple_requirement(n, "^4.17.20", "4.17.21").unwrap(),
            "^4.17.21"
        );
        assert_eq!(
            bump_simple_requirement(n, "~1.2.3", "1.2.9").unwrap(),
            "~1.2.9"
        );
        assert_eq!(
            bump_simple_requirement(n, "1.0.0", "1.0.1").unwrap(),
            "1.0.1"
        );
        assert_eq!(
            bump_simple_requirement(n, ">=1.0.0", "2.0.0").unwrap(),
            ">=2.0.0"
        );
        assert_eq!(
            bump_simple_requirement(n, "=1.0.0", "1.0.1").unwrap(),
            "=1.0.1"
        );
        assert!(bump_simple_requirement(n, "^1.0.0 || ^2.0.0", "2.0.1").is_err());
        assert!(bump_simple_requirement(n, "<2.0.0", "2.0.1").is_err());
        assert!(bump_simple_requirement(n, "1.x", "1.2.3").is_err());
        assert!(bump_simple_requirement(n, "latest", "1.2.3").is_err());
        assert!(bump_simple_requirement(n, "git+https://example.com/x", "1.2.3").is_err());
        assert_eq!(
            bump_simple_requirement(Ecosystem::Cargo, "1.2", "1.4.2").unwrap(),
            "1.4.2"
        );
    }

    #[test]
    fn json_spans_find_dependency_versions() {
        let text = "{\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"lodash\": \"^4.17.20\",\n    \"a\\\"b\": \"1.0.0\"\n  },\n  \"devDependencies\": { \"jest\": \"29.0.0\" },\n  \"files\": [\"lodash\"]\n}\n";
        let d = npm_decls("package.json", text, "lodash");
        assert_eq!(d.len(), 1);
        assert_eq!(&text[d[0].span.clone()], "^4.17.20");
        assert_eq!(npm_decls("package.json", text, "jest").len(), 1);
        assert!(npm_decls("package.json", text, "missing").is_empty());
        assert!(npm_decls("package.json", "{broken", "lodash").is_empty());
    }

    #[test]
    fn cargo_spans_cover_plain_table_workspace_and_renamed_forms() {
        let text = "[workspace.dependencies]\nserde = { version = \"1.0.100\", features = [\"derive\"] }\n\n[dependencies]\nregex = \"1.5\"\nold-name = { package = \"memchr\", version = '2.4' }\nlocal = { path = \"../local\" }\n\n[target.'cfg(unix)'.dev-dependencies]\nlibc = \"0.2\"\n";
        let get = |n: &str| -> Vec<String> {
            cargo_decls("Cargo.toml", text, n)
                .iter()
                .map(|d| text[d.span.clone()].to_owned())
                .collect()
        };
        assert_eq!(get("serde"), vec!["1.0.100"]);
        assert_eq!(get("regex"), vec!["1.5"]);
        assert_eq!(get("memchr"), vec!["2.4"]);
        assert_eq!(get("libc"), vec!["0.2"]);
        assert!(get("local").is_empty());
    }

    #[test]
    fn pip_edit_rewrites_the_pinned_version_in_place() {
        let text = "# pins\nrequests==2.25.0 \\\n    --hash=sha256:aa\nDjango>=3.2,<4.0 ; python_version > '3.7'\nflask~=2.0\n";
        let e = pip_edit("r.txt", text, "requests", "2.31.0")
            .unwrap()
            .unwrap();
        assert_eq!((e.old.as_str(), e.new.as_str()), ("2.25.0", "2.31.0"));
        assert_eq!(&text[e.start..e.end], "2.25.0");
        let d = pip_edit("r.txt", text, "django", "3.2.25")
            .unwrap()
            .unwrap();
        assert_eq!(&text[d.start..d.end], "3.2");
        let f = pip_edit("r.txt", text, "Flask", "2.3.2").unwrap().unwrap();
        assert_eq!(&text[f.start..f.end], "2.0");
        assert!(
            pip_edit("r.txt", text, "django", "4.2").is_err(),
            "the < 4.0 cap rules out 4.2"
        );
        assert_eq!(pip_edit("r.txt", text, "absent", "1.0").unwrap(), None);
    }

    #[test]
    fn go_edit_handles_blocks_and_single_lines() {
        let text = "module m\n\nrequire (\n\tgithub.com/a/b v1.2.3\n\tgolang.org/x/text v0.3.6 // indirect\n)\n\nrequire github.com/c/d v0.1.0\n";
        let e = go_edit("go.mod", text, "golang.org/x/text", "v0.3.6", "v0.3.8").unwrap();
        assert_eq!(&text[e.start..e.end], "v0.3.6");
        let d = go_edit("go.mod", text, "github.com/c/d", "v0.1.0", "v0.2.0").unwrap();
        assert_eq!(&text[d.start..d.end], "v0.1.0");
        assert!(go_edit("go.mod", text, "github.com/a/b", "v9.9.9", "v10").is_none());
    }

    #[test]
    fn apply_edits_checks_old_text_and_overlap() {
        let e = |s, t, old: &str, new: &str| TextEdit {
            path: "f".into(),
            start: s,
            end: t,
            old: old.into(),
            new: new.into(),
        };
        assert_eq!(
            apply_edits("a1 b2 c3", &[e(1, 2, "1", "X"), e(7, 8, "3", "Z")]).unwrap(),
            "aX b2 cZ"
        );
        assert!(apply_edits("abc", &[e(0, 1, "z", "y")]).is_err());
        assert!(apply_edits("abcd", &[e(0, 2, "ab", "x"), e(1, 3, "bc", "y")]).is_err());
    }
}
