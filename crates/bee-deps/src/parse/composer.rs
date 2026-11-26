//! Composer: `composer.lock` and `composer.json`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::build::{self, Raw, RawDep, Root};
use super::{In, Kind, Out, dir_of, nearest, within};
use crate::error::{DepsError, Result};
use crate::model::{Ecosystem, Scope};

fn is_platform(name: &str) -> bool {
    matches!(
        name,
        "php"
            | "php-64bit"
            | "php-ipv6"
            | "php-zts"
            | "php-debug"
            | "hhvm"
            | "composer"
            | "composer-plugin-api"
            | "composer-runtime-api"
    ) || name.starts_with("ext-")
        || name.starts_with("lib-")
}

fn clean_version(v: &str) -> &str {
    v.strip_prefix('v')
        .filter(|r| r.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(v)
}

fn parse_json(path: &str, text: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(text)
        .map_err(|e| DepsError::invalid(path, format!("invalid JSON: {e}")))?;
    if !v.is_object() {
        return Err(DepsError::invalid(path, "expected a JSON object"));
    }
    Ok(v)
}

fn parse_composer_json(path: &str, text: &str) -> Result<Vec<Root>> {
    let doc = parse_json(path, text)?;
    let mut roots = Vec::new();
    for (key, scope) in [
        ("require", Scope::Runtime),
        ("require-dev", Scope::Development),
    ] {
        if let Some(m) = doc.get(key).and_then(Value::as_object) {
            for (name, req) in m {
                if !is_platform(name) {
                    roots.push(Root::new(&name.to_lowercase(), req.as_str(), scope));
                }
            }
        }
    }
    Ok(roots)
}

fn parse_composer_lock(path: &str, text: &str) -> Result<(Vec<Raw>, Vec<Root>)> {
    let doc = parse_json(path, text)?;
    let mut raw = Vec::new();
    for (key, dev) in [("packages", false), ("packages-dev", true)] {
        for p in doc.get(key).and_then(Value::as_array).into_iter().flatten() {
            let (Some(name), Some(version)) = (
                p.get("name").and_then(Value::as_str),
                p.get("version").and_then(Value::as_str),
            ) else {
                continue;
            };
            let mut r = Raw::new(&name.to_lowercase(), clean_version(version));
            r.dev_hint = Some(dev);
            if let Some(m) = p.get("require").and_then(Value::as_object) {
                for (dn, req) in m {
                    if !is_platform(dn) {
                        r.deps.push(RawDep::new(&dn.to_lowercase(), req.as_str()));
                    }
                }
            }
            r.licenses = p
                .get("license")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            r.integrity = p
                .get("dist")
                .and_then(|d| d.get("shasum"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            raw.push(r);
        }
    }
    Ok((raw, Vec::new()))
}

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    let mut manifests: BTreeMap<&str, Vec<Root>> = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::ComposerJson) {
        match parse_composer_json(i.path, i.content) {
            Ok(r) => {
                manifests.insert(i.path, r);
            }
            Err(e) => out.failed(i, &e),
        }
    }
    let locks: Vec<&In<'_>> = inputs
        .iter()
        .filter(|i| i.kind == Kind::ComposerLock)
        .collect();
    let lock_dirs: Vec<&str> = locks.iter().map(|l| dir_of(l.path)).collect();
    let mut assigned: BTreeSet<&str> = BTreeSet::new();
    for l in &locks {
        match parse_composer_lock(l.path, l.content) {
            Ok((raw, _)) => {
                let dir = dir_of(l.path);
                let mut roots = Vec::new();
                for (path, rr) in &manifests {
                    if within(dir, path) && nearest(&lock_dirs, path) == Some(dir) {
                        assigned.insert(path);
                        roots.extend(rr.iter().cloned());
                    }
                }
                let pk = build::build(Ecosystem::Composer, l.path, raw, roots, true);
                out.add(l, pk);
            }
            Err(e) => out.failed(l, &e),
        }
    }
    for i in inputs.iter().filter(|i| i.kind == Kind::ComposerJson) {
        let Some(roots) = manifests.get(i.path) else {
            continue;
        };
        if assigned.contains(i.path) {
            out.file(i, roots.len());
        } else {
            let pk = build::build(
                Ecosystem::Composer,
                i.path,
                Vec::new(),
                roots.clone(),
                false,
            );
            out.add(i, pk);
        }
    }
    out
}
