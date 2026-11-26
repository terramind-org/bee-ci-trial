//! Cargo: `Cargo.toml` and `Cargo.lock`.

use std::collections::{BTreeMap, BTreeSet};

use toml::Value;

use super::build::{self, Raw, RawDep, Root};
use super::{In, Kind, Out, dir_of, nearest, within};
use crate::error::{DepsError, Result};
use crate::model::{Ecosystem, Scope};

struct Manifest {
    roots: Vec<Root>,
    /// `[workspace.dependencies]` requirements by name.
    workspace: BTreeMap<String, String>,
}

type DepTable<'a> = (Scope, &'a toml::map::Map<String, Value>);

fn collect_tables<'a>(v: &'a Value, out: &mut Vec<DepTable<'a>>) {
    for (key, scope) in [
        ("dependencies", Scope::Runtime),
        ("build-dependencies", Scope::Runtime),
        ("dev-dependencies", Scope::Development),
    ] {
        if let Some(t) = v.get(key).and_then(Value::as_table) {
            out.push((scope, t));
        }
    }
}

fn dep_tables(doc: &Value) -> Vec<DepTable<'_>> {
    let mut out = Vec::new();
    collect_tables(doc, &mut out);
    if let Some(targets) = doc.get("target").and_then(Value::as_table) {
        for t in targets.values() {
            collect_tables(t, &mut out);
        }
    }
    out
}

fn parse_manifest(path: &str, text: &str) -> Result<Manifest> {
    let doc: Value =
        toml::from_str(text).map_err(|e| DepsError::invalid(path, format!("invalid TOML: {e}")))?;
    let mut workspace = BTreeMap::new();
    if let Some(t) = doc
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(Value::as_table)
    {
        for (name, v) in t {
            let req = match v {
                Value::String(s) => Some(s.clone()),
                Value::Table(t) => t.get("version").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            };
            if let Some(r) = req {
                let real = v.get("package").and_then(Value::as_str).unwrap_or(name);
                workspace.insert(real.to_owned(), r);
            }
        }
    }
    let mut roots = Vec::new();
    for (scope, table) in dep_tables(&doc) {
        for (name, v) in table {
            let (real, req, local) = match v {
                Value::String(s) => (name.as_str(), Some(s.clone()), false),
                Value::Table(t) => {
                    let real = t.get("package").and_then(Value::as_str).unwrap_or(name);
                    let req = t.get("version").and_then(Value::as_str).map(str::to_owned);
                    let ws = t.get("workspace").and_then(Value::as_bool).unwrap_or(false);
                    let req = if ws {
                        Some(format!("\u{0}workspace:{real}"))
                    } else {
                        req
                    };
                    let local = (t.contains_key("path") || t.contains_key("git")) && req.is_none();
                    (real, req, local)
                }
                _ => continue,
            };
            if local {
                continue;
            }
            roots.push(Root::new(real, req.as_deref(), scope));
        }
    }
    Ok(Manifest { roots, workspace })
}

struct Lock {
    raw: Vec<Raw>,
    /// Dependencies of local (workspace) packages, as roots.
    local_roots: Vec<Root>,
    local_names: BTreeSet<String>,
}

fn parse_lock(path: &str, text: &str) -> Result<Lock> {
    let doc: Value =
        toml::from_str(text).map_err(|e| DepsError::invalid(path, format!("invalid TOML: {e}")))?;
    let pkgs = doc
        .get("package")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut raw = Vec::new();
    let mut local_roots = Vec::new();
    let mut local_names = BTreeSet::new();
    for p in &pkgs {
        let name = p
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| DepsError::invalid(path, "package without a name"))?;
        let version = p.get("version").and_then(Value::as_str).unwrap_or("");
        let deps: Vec<RawDep> = p
            .get("dependencies")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| {
                        let mut it = s.split_whitespace();
                        let n = it.next().unwrap_or("");
                        RawDep::new(n, it.next())
                    })
                    .collect()
            })
            .unwrap_or_default();
        if p.get("source").is_none() {
            local_names.insert(name.to_owned());
            for d in &deps {
                local_roots.push(Root::new(&d.name, d.req.as_deref(), Scope::Runtime));
            }
            continue;
        }
        let mut r = Raw::new(name, version);
        r.deps = deps;
        r.integrity = p
            .get("checksum")
            .and_then(Value::as_str)
            .map(|c| format!("sha256:{c}"));
        raw.push(r);
    }
    Ok(Lock {
        raw,
        local_roots,
        local_names,
    })
}

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    let mut manifests: BTreeMap<&str, Manifest> = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::CargoToml) {
        match parse_manifest(i.path, i.content) {
            Ok(m) => {
                manifests.insert(i.path, m);
            }
            Err(e) => out.failed(i, &e),
        }
    }
    let ws_dirs: Vec<&str> = manifests
        .iter()
        .filter(|(_, m)| !m.workspace.is_empty())
        .map(|(p, _)| dir_of(p))
        .collect();
    // Resolve `workspace = true` requirements.
    let mut resolved_roots: BTreeMap<&str, Vec<Root>> = BTreeMap::new();
    for (path, m) in &manifests {
        let ws = nearest(&ws_dirs, path).and_then(|d| {
            manifests
                .iter()
                .find(|(p, mm)| dir_of(p) == d && !mm.workspace.is_empty())
                .map(|(_, mm)| mm)
        });
        let roots = m
            .roots
            .iter()
            .map(|r| {
                let mut r = r.clone();
                if r.requirement
                    .as_deref()
                    .is_some_and(|q| q.starts_with('\u{0}'))
                {
                    r.requirement = ws.and_then(|w| w.workspace.get(&r.name).cloned());
                }
                r
            })
            .collect();
        resolved_roots.insert(path, roots);
    }
    let locks: Vec<&In<'_>> = inputs
        .iter()
        .filter(|i| i.kind == Kind::CargoLock)
        .collect();
    let lock_dirs: Vec<&str> = locks.iter().map(|l| dir_of(l.path)).collect();
    let mut assigned: BTreeSet<&str> = BTreeSet::new();
    for l in &locks {
        let lock = match parse_lock(l.path, l.content) {
            Ok(l) => l,
            Err(e) => {
                out.failed(l, &e);
                continue;
            }
        };
        let dir = dir_of(l.path);
        let mut roots: Vec<Root> = Vec::new();
        for (path, rr) in &resolved_roots {
            if within(dir, path) && nearest(&lock_dirs, path) == Some(dir) {
                assigned.insert(path);
                roots.extend(
                    rr.iter()
                        .filter(|r| !lock.local_names.contains(&r.name))
                        .cloned(),
                );
            }
        }
        if roots.is_empty() {
            roots = lock.local_roots.clone();
        }
        let pk = build::build(Ecosystem::Cargo, l.path, lock.raw, roots, true);
        out.add(l, pk);
    }
    for i in inputs.iter().filter(|i| i.kind == Kind::CargoToml) {
        let Some(roots) = resolved_roots.get(i.path) else {
            continue;
        };
        let count = roots.len();
        if assigned.contains(i.path) {
            out.file(i, count);
        } else {
            let pk = build::build(Ecosystem::Cargo, i.path, Vec::new(), roots.clone(), false);
            out.add(i, pk);
        }
    }
    out
}
