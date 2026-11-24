//! Python: `requirements*.txt`, `poetry.lock`, `uv.lock`, `Pipfile.lock` and `pyproject.toml`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use toml::Value;

use super::build::{self, Raw, RawDep, Root};
use super::{In, Kind, Out, dir_of, nearest, within};
use crate::error::{DepsError, Result};
use crate::model::{Ecosystem, Scope};
use crate::version::{Pep440, Specifiers};

/// A parsed PEP 508 requirement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Requirement {
    pub name: String,
    pub spec: String,
    pub marker: Option<String>,
    pub url: bool,
}

/// Parses `name[extra]>=1.0,<2; python_version < "3.9"` or `name @ https://...`.
pub(crate) fn parse_pep508(text: &str) -> Option<Requirement> {
    let t = text.trim();
    let (main, marker) = match t.split_once(';') {
        Some((m, k)) => (
            m.trim(),
            Some(k.trim().to_owned()).filter(|k| !k.is_empty()),
        ),
        None => (t, None),
    };
    let name_end = main
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(main.len());
    let name = &main[..name_end];
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return None;
    }
    let mut rest = main[name_end..].trim_start();
    if rest.starts_with('[') {
        let end = rest.find(']')?;
        rest = rest[end + 1..].trim_start();
    }
    if let Some(url) = rest.strip_prefix('@') {
        let _ = url;
        return Some(Requirement {
            name: name.to_owned(),
            spec: String::new(),
            marker,
            url: true,
        });
    }
    let rest = rest.trim_start_matches('(').trim_end_matches(')').trim();
    let spec: String = rest.split_whitespace().collect::<Vec<_>>().join("");
    Some(Requirement {
        name: name.to_owned(),
        spec,
        marker,
        url: false,
    })
}

fn pinned_version(spec: &str) -> Option<String> {
    let s = Specifiers::parse(spec)?;
    let p = s.pinned()?;
    Pep440::parse(&p).map(|_| p)
}

fn scope_from_name(name: &str) -> Scope {
    let lower = name.to_ascii_lowercase();
    let dev = lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
        matches!(
            t,
            "dev"
                | "test"
                | "tests"
                | "testing"
                | "lint"
                | "docs"
                | "doc"
                | "ci"
                | "typing"
                | "mypy"
        )
    });
    if dev {
        Scope::Development
    } else {
        Scope::Runtime
    }
}

/// Converts a Poetry constraint (`^1.2`, `~1.2.3`, `1.2.*`) to PEP 440 specifiers.
pub(crate) fn poetry_to_pep440(req: &str) -> String {
    let t = req.trim();
    if t.is_empty() || t == "*" {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    for p in t.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let nums = |s: &str| -> Option<Vec<u64>> {
            let core = s.split(['-', '+', 'a', 'b', 'r']).next().unwrap_or(s);
            core.split('.').map(|x| x.parse().ok()).collect()
        };
        let bump = |v: &[u64], i: usize| -> String {
            let mut up: Vec<u64> = v.iter().take(i).copied().collect();
            up.push(v.get(i).copied().unwrap_or(0) + 1);
            up.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
        };
        if let Some(v) = p.strip_prefix('^') {
            match nums(v.trim()) {
                Some(n) if !n.is_empty() => {
                    let i = n.iter().position(|x| *x != 0).unwrap_or(n.len() - 1);
                    parts.push(format!(">={}", v.trim()));
                    parts.push(format!("<{}", bump(&n, i)));
                }
                _ => parts.push(p.to_owned()),
            }
        } else if let Some(v) = p.strip_prefix('~').filter(|v| !v.starts_with('=')) {
            match nums(v.trim()) {
                Some(n) if !n.is_empty() => {
                    let i = if n.len() >= 2 { 1 } else { 0 };
                    parts.push(format!(">={}", v.trim()));
                    parts.push(format!("<{}", bump(&n, i)));
                }
                _ => parts.push(p.to_owned()),
            }
        } else if p.starts_with(['=', '<', '>', '!', '~']) {
            parts.push(p.to_owned());
        } else {
            parts.push(format!("=={p}"));
        }
    }
    parts.join(",")
}

fn is_dev_table(name: &str) -> bool {
    name != "main"
}

/// Roots from a `pyproject.toml`: PEP 621, PEP 735 and Poetry tables.
fn parse_pyproject(path: &str, text: &str) -> Result<Vec<Root>> {
    let doc: Value =
        toml::from_str(text).map_err(|e| DepsError::invalid(path, format!("invalid TOML: {e}")))?;
    let mut roots = Vec::new();
    let mut add_pep508 = |list: &[Value], scope: Scope| {
        for v in list {
            if let Some(r) = v.as_str().and_then(parse_pep508) {
                let mut root = Root::new(&r.name, Some(&r.spec), scope);
                root.pinned = pinned_version(&r.spec);
                roots.push(root);
            }
        }
    };
    if let Some(project) = doc.get("project") {
        if let Some(a) = project.get("dependencies").and_then(Value::as_array) {
            add_pep508(a, Scope::Runtime);
        }
        if let Some(t) = project
            .get("optional-dependencies")
            .and_then(Value::as_table)
        {
            for (group, list) in t {
                if let Some(a) = list.as_array() {
                    add_pep508(a, scope_from_name(group));
                }
            }
        }
    }
    if let Some(t) = doc.get("dependency-groups").and_then(Value::as_table) {
        for list in t.values() {
            if let Some(a) = list.as_array() {
                add_pep508(a, Scope::Development);
            }
        }
    }
    if let Some(a) = doc
        .get("tool")
        .and_then(|t| t.get("uv"))
        .and_then(|u| u.get("dev-dependencies"))
        .and_then(Value::as_array)
    {
        add_pep508(a, Scope::Development);
    }
    if let Some(poetry) = doc.get("tool").and_then(|t| t.get("poetry")) {
        let mut add_table = |t: &toml::map::Map<String, Value>, scope: Scope| {
            for (name, v) in t {
                if name.eq_ignore_ascii_case("python") {
                    continue;
                }
                let req = match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Table(t) => t.get("version").and_then(Value::as_str).map(str::to_owned),
                    Value::Array(a) => a
                        .first()
                        .and_then(|x| x.get("version"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    _ => None,
                };
                let local = v.get("path").is_some() || v.get("git").is_some();
                if local && req.is_none() {
                    continue;
                }
                let spec = req.as_deref().map(poetry_to_pep440);
                let mut root = Root::new(name, spec.as_deref(), scope);
                root.pinned = spec.as_deref().and_then(pinned_version);
                roots.push(root);
            }
        };
        if let Some(t) = poetry.get("dependencies").and_then(Value::as_table) {
            add_table(t, Scope::Runtime);
        }
        if let Some(t) = poetry.get("dev-dependencies").and_then(Value::as_table) {
            add_table(t, Scope::Development);
        }
        if let Some(groups) = poetry.get("group").and_then(Value::as_table) {
            for (g, body) in groups {
                if let Some(t) = body.get("dependencies").and_then(Value::as_table) {
                    add_table(
                        t,
                        if is_dev_table(g) {
                            Scope::Development
                        } else {
                            Scope::Runtime
                        },
                    );
                }
            }
        }
    }
    Ok(roots)
}

struct LockData {
    raw: Vec<Raw>,
    roots: Vec<Root>,
}

fn first_hash(files: Option<&Value>) -> Option<String> {
    files?
        .as_array()?
        .iter()
        .find_map(|f| f.get("hash").and_then(Value::as_str))
        .map(str::to_owned)
}

fn parse_poetry_lock(path: &str, text: &str) -> Result<LockData> {
    let doc: Value =
        toml::from_str(text).map_err(|e| DepsError::invalid(path, format!("invalid TOML: {e}")))?;
    let mut raw = Vec::new();
    for p in doc
        .get("package")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = p.get("name").and_then(Value::as_str) else {
            continue;
        };
        let version = p.get("version").and_then(Value::as_str).unwrap_or("");
        let mut r = Raw::new(name, version);
        if let Some(t) = p.get("dependencies").and_then(Value::as_table) {
            for (dn, dv) in t {
                let req = match dv {
                    Value::String(s) => Some(s.clone()),
                    Value::Table(t) => t.get("version").and_then(Value::as_str).map(str::to_owned),
                    Value::Array(a) => a
                        .first()
                        .and_then(|x| x.get("version"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    _ => None,
                };
                r.deps.push(RawDep::new(
                    dn,
                    req.as_deref().map(poetry_to_pep440).as_deref(),
                ));
            }
        }
        r.integrity = first_hash(p.get("files"));
        if let Some(c) = p.get("category").and_then(Value::as_str) {
            r.dev_hint = Some(c == "dev");
        } else if let Some(g) = p.get("groups").and_then(Value::as_array) {
            r.dev_hint = Some(!g.iter().any(|x| x.as_str() == Some("main")));
        }
        raw.push(r);
    }
    Ok(LockData {
        raw,
        roots: Vec::new(),
    })
}

fn parse_uv_lock(path: &str, text: &str) -> Result<LockData> {
    let doc: Value =
        toml::from_str(text).map_err(|e| DepsError::invalid(path, format!("invalid TOML: {e}")))?;
    let dep_list = |v: Option<&Value>| -> Vec<RawDep> {
        v.and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|d| {
                let n = d.get("name").and_then(Value::as_str)?;
                Some(RawDep::new(n, d.get("version").and_then(Value::as_str)))
            })
            .collect()
    };
    let mut raw = Vec::new();
    let mut roots = Vec::new();
    for p in doc
        .get("package")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = p.get("name").and_then(Value::as_str) else {
            continue;
        };
        let source = p.get("source");
        let local = source.is_some_and(|s| {
            ["virtual", "editable", "directory", "path"]
                .iter()
                .any(|k| s.get(k).is_some())
        });
        let mut deps = dep_list(p.get("dependencies"));
        if let Some(t) = p.get("optional-dependencies").and_then(Value::as_table) {
            for list in t.values() {
                deps.extend(dep_list(Some(list)));
            }
        }
        if local {
            for d in &deps {
                roots.push(Root::new(&d.name, None, Scope::Runtime));
            }
            if let Some(t) = p.get("dev-dependencies").and_then(Value::as_table) {
                for list in t.values() {
                    for d in dep_list(Some(list)) {
                        roots.push(Root::new(&d.name, None, Scope::Development));
                    }
                }
            }
            continue;
        }
        let version = p.get("version").and_then(Value::as_str).unwrap_or("");
        let mut r = Raw::new(name, version);
        r.deps = deps;
        r.integrity = p
            .get("sdist")
            .and_then(|s| s.get("hash"))
            .and_then(Value::as_str)
            .or_else(|| {
                p.get("wheels")
                    .and_then(Value::as_array)
                    .and_then(|w| w.first())
                    .and_then(|w| w.get("hash"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);
        raw.push(r);
    }
    Ok(LockData { raw, roots })
}

fn parse_pipfile_lock(path: &str, text: &str) -> Result<LockData> {
    let doc: Json = serde_json::from_str(text)
        .map_err(|e| DepsError::invalid(path, format!("invalid JSON: {e}")))?;
    let mut raw = Vec::new();
    let mut roots = Vec::new();
    for (section, scope) in [("default", Scope::Runtime), ("develop", Scope::Development)] {
        let Some(m) = doc.get(section).and_then(Json::as_object) else {
            continue;
        };
        for (name, v) in m {
            let Some(version) = v.get("version").and_then(Json::as_str) else {
                continue;
            };
            let version = version.trim_start_matches('=').trim();
            let mut r = Raw::new(name, version);
            r.integrity = v
                .get("hashes")
                .and_then(Json::as_array)
                .and_then(|h| h.first())
                .and_then(Json::as_str)
                .map(str::to_owned);
            r.dev_hint = Some(scope == Scope::Development);
            raw.push(r);
            roots.push(Root::new(name, Some(&format!("=={version}")), scope).pinned(version));
        }
    }
    Ok(LockData { raw, roots })
}

/// Parses a requirements file into `(requirement, first hash)` pairs.
fn parse_requirements(text: &str) -> Vec<(Requirement, Option<String>)> {
    let mut logical: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let l = line.trim_end();
        if let Some(stripped) = l.strip_suffix('\\') {
            cur.push_str(stripped);
            cur.push(' ');
            continue;
        }
        cur.push_str(l);
        logical.push(std::mem::take(&mut cur));
    }
    if !cur.is_empty() {
        logical.push(cur);
    }
    let mut out = Vec::new();
    for line in logical {
        let mut l = line.trim().to_owned();
        if l.starts_with('#') {
            continue;
        }
        if let Some(i) = l.find(" #") {
            l.truncate(i);
        }
        let l = l.trim();
        if l.is_empty()
            || l.starts_with('-')
            || l.starts_with("git+")
            || l.contains("://") && !l.contains(" @ ")
        {
            continue;
        }
        let mut hash = None;
        let mut words: Vec<&str> = Vec::new();
        let mut rest_is_options = false;
        for w in l.split_whitespace() {
            if let Some(h) = w.strip_prefix("--hash=") {
                hash.get_or_insert_with(|| h.to_owned());
                rest_is_options = true;
            } else if w.starts_with("--") {
                rest_is_options = true;
            } else if !rest_is_options {
                words.push(w);
            }
        }
        let text = words.join(" ");
        if let Some(r) = parse_pep508(&text) {
            out.push((r, hash));
        }
    }
    out
}

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    // pyproject roots.
    let mut pyprojects: BTreeMap<&str, Vec<Root>> = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::PyProject) {
        match parse_pyproject(i.path, i.content) {
            Ok(r) => {
                pyprojects.insert(i.path, r);
            }
            Err(e) => out.failed(i, &e),
        }
    }
    let locks: Vec<&In<'_>> = inputs
        .iter()
        .filter(|i| matches!(i.kind, Kind::PoetryLock | Kind::UvLock))
        .collect();
    let lock_dirs: Vec<&str> = locks.iter().map(|l| dir_of(l.path)).collect();
    let mut assigned: BTreeSet<&str> = BTreeSet::new();
    for l in &locks {
        let data = if l.kind == Kind::PoetryLock {
            parse_poetry_lock(l.path, l.content)
        } else {
            parse_uv_lock(l.path, l.content)
        };
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                out.failed(l, &e);
                continue;
            }
        };
        let dir = dir_of(l.path);
        let mut roots = Vec::new();
        for (path, rr) in &pyprojects {
            if within(dir, path) && nearest(&lock_dirs, path) == Some(dir) {
                assigned.insert(path);
                roots.extend(rr.iter().cloned().map(|mut r| {
                    r.pinned = None; // the lockfile decides the version
                    r
                }));
            }
        }
        if roots.is_empty() {
            roots = data.roots;
        }
        let pk = build::build(Ecosystem::PyPI, l.path, data.raw, roots, true);
        out.add(l, pk);
    }
    for i in inputs.iter().filter(|i| i.kind == Kind::PipfileLock) {
        match parse_pipfile_lock(i.path, i.content) {
            Ok(d) => {
                let pk = build::build(Ecosystem::PyPI, i.path, d.raw, d.roots, true);
                out.add(i, pk);
            }
            Err(e) => out.failed(i, &e),
        }
    }
    for i in inputs.iter().filter(|i| i.kind == Kind::Requirements) {
        let reqs = parse_requirements(i.content);
        let scope = scope_from_name(i.path);
        let mut roots = Vec::new();
        let mut hashes: BTreeMap<String, String> = BTreeMap::new();
        let mut all_pinned = !reqs.is_empty();
        for (r, h) in &reqs {
            let pinned = pinned_version(&r.spec);
            all_pinned &= pinned.is_some();
            let mut root = Root::new(&r.name, Some(&r.spec), scope);
            root.pinned = pinned;
            roots.push(root);
            if let Some(h) = h {
                hashes.insert(Ecosystem::PyPI.normalize_name(&r.name), h.clone());
            }
        }
        let mut pk = build::build(Ecosystem::PyPI, i.path, Vec::new(), roots, false);
        for p in &mut pk {
            p.integrity = hashes.get(&p.normalized_name()).cloned();
        }
        out.file_as(i, pk.len(), all_pinned);
        out.packages.extend(pk);
    }
    for i in inputs.iter().filter(|i| i.kind == Kind::PyProject) {
        let Some(roots) = pyprojects.get(i.path) else {
            continue;
        };
        if assigned.contains(i.path) {
            out.file(i, roots.len());
        } else {
            let pk = build::build(Ecosystem::PyPI, i.path, Vec::new(), roots.clone(), false);
            out.add(i, pk);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pep508_forms() {
        let r = parse_pep508("Django[argon2]>=3.2, <4.0 ; python_version >= '3.8'").unwrap();
        assert_eq!(r.name, "Django");
        assert_eq!(r.spec, ">=3.2,<4.0");
        assert_eq!(r.marker.as_deref(), Some("python_version >= '3.8'"));
        assert!(parse_pep508("pkg @ https://example.com/p.whl").unwrap().url);
        assert_eq!(parse_pep508("requests (==2.0.0)").unwrap().spec, "==2.0.0");
        assert!(parse_pep508("==1.0").is_none());
    }

    #[test]
    fn poetry_constraints_convert() {
        assert_eq!(poetry_to_pep440("^1.2.3"), ">=1.2.3,<2");
        assert_eq!(poetry_to_pep440("^0.2.3"), ">=0.2.3,<0.3");
        assert_eq!(poetry_to_pep440("^0.0.3"), ">=0.0.3,<0.0.4");
        assert_eq!(poetry_to_pep440("~1.2.3"), ">=1.2.3,<1.3");
        assert_eq!(poetry_to_pep440("~1"), ">=1,<2");
        assert_eq!(poetry_to_pep440("1.2.*"), "==1.2.*");
        assert_eq!(poetry_to_pep440(">=1.0,<2.0"), ">=1.0,<2.0");
        assert_eq!(poetry_to_pep440("*"), "");
        assert_eq!(poetry_to_pep440("2.0.1"), "==2.0.1");
    }

    #[test]
    fn requirements_with_hashes_markers_and_continuations() {
        let text = "# c\n-r base.txt\n--index-url https://x\nrequests==2.28.1 \\\n    --hash=sha256:aaa \\\n    --hash=sha256:bbb\nflask>=2.0 ; python_version > '3.7'  # comment\n-e git+https://example.com/x.git#egg=x\nnumpy\n";
        let r = parse_requirements(text);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].0.name, "requests");
        assert_eq!(r[0].1.as_deref(), Some("sha256:aaa"));
        assert_eq!(r[1].0.marker.as_deref(), Some("python_version > '3.7'"));
        assert_eq!(r[2].0.spec, "");
    }
}
