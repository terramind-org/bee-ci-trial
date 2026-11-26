//! npm: `package.json`, `package-lock.json` (v1, v2, v3), `npm-shrinkwrap.json`,
//! `pnpm-lock.yaml` (v5, v6, v9) and `yarn.lock` (v1 and Berry).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::build::{self, Raw, RawDep, Root};
use super::{In, Kind, Out, dir_of, nearest, within};
use crate::error::{DepsError, Result};
use crate::model::{Ecosystem, Scope};
use crate::yaml;

/// A lockfile read into raw entries plus whatever roots it declares itself.
#[derive(Default)]
struct LockData {
    raw: Vec<Raw>,
    roots: Vec<Root>,
    /// `name@range` descriptors to resolved versions (Yarn), used to pin manifest roots.
    descriptors: BTreeMap<String, String>,
}

fn json_obj<'a>(v: &'a Value, key: &str) -> Option<&'a serde_json::Map<String, Value>> {
    v.get(key).and_then(Value::as_object)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn parse_package_json(path: &str, text: &str) -> Result<Vec<Root>> {
    let doc: Value = serde_json::from_str(text)
        .map_err(|e| DepsError::invalid(path, format!("invalid JSON: {e}")))?;
    if !doc.is_object() {
        return Err(DepsError::invalid(path, "package.json must be an object"));
    }
    let mut roots = Vec::new();
    for (key, scope) in [
        ("dependencies", Scope::Runtime),
        ("optionalDependencies", Scope::Runtime),
        ("devDependencies", Scope::Development),
    ] {
        if let Some(m) = json_obj(&doc, key) {
            for (name, req) in m {
                if let Some(r) = req.as_str() {
                    roots.push(Root::new(name, Some(r), scope));
                }
            }
        }
    }
    Ok(roots)
}

// ---------------------------------------------------------------------------------------------
// package-lock.json
// ---------------------------------------------------------------------------------------------

struct Entry {
    key: String,
    name: String,
    version: String,
    dev: bool,
    license: Option<String>,
    integrity: Option<String>,
    deps: Vec<(String, String)>,
    link: bool,
}

fn name_from_key(key: &str) -> String {
    match key.rfind("node_modules/") {
        Some(i) => key[i + "node_modules/".len()..].to_owned(),
        None => key.rsplit('/').next().unwrap_or(key).to_owned(),
    }
}

/// The key of the entry a dependency of `owner` resolves to, following Node's lookup order.
fn locate(keys: &BTreeMap<String, usize>, owner: &str, name: &str) -> Option<usize> {
    let mut base = owner.to_owned();
    loop {
        let cand = if base.is_empty() {
            format!("node_modules/{name}")
        } else {
            format!("{base}/node_modules/{name}")
        };
        if let Some(i) = keys.get(&cand) {
            return Some(*i);
        }
        if base.is_empty() {
            return None;
        }
        base = match base.rfind("node_modules/") {
            Some(0) | None => String::new(),
            Some(i) => base[..i - 1].to_owned(),
        };
    }
}

fn flatten_v1(prefix: &str, deps: &serde_json::Map<String, Value>, out: &mut Vec<Entry>) {
    for (name, v) in deps {
        let key = format!("{prefix}node_modules/{name}");
        let requires = json_obj(v, "requires")
            .map(|m| {
                m.iter()
                    .filter_map(|(k, r)| r.as_str().map(|r| (k.clone(), r.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        out.push(Entry {
            key: key.clone(),
            name: name.clone(),
            version: str_of(v, "version").unwrap_or("").to_owned(),
            dev: v.get("dev").and_then(Value::as_bool).unwrap_or(false),
            license: None,
            integrity: str_of(v, "integrity").map(str::to_owned),
            deps: requires,
            link: false,
        });
        if let Some(nested) = json_obj(v, "dependencies") {
            flatten_v1(&format!("{key}/"), nested, out);
        }
    }
}

fn parse_package_lock(path: &str, text: &str) -> Result<LockData> {
    let doc: Value = serde_json::from_str(text)
        .map_err(|e| DepsError::invalid(path, format!("invalid JSON: {e}")))?;
    let mut entries: Vec<Entry> = Vec::new();
    // `(owner key, declared dependencies with scope)` for the root and workspace members.
    let mut decls: Vec<(String, Vec<(String, Scope)>)> = Vec::new();
    let mut have_roots = false;
    if let Some(pkgs) = json_obj(&doc, "packages") {
        for (key, v) in pkgs {
            let mut declared: Vec<(String, Scope)> = Vec::new();
            for (field, scope) in [
                ("dependencies", Scope::Runtime),
                ("optionalDependencies", Scope::Runtime),
                ("devDependencies", Scope::Development),
            ] {
                if let Some(m) = json_obj(v, field) {
                    declared.extend(m.keys().map(|n| (n.clone(), scope)));
                }
            }
            let is_member = key.is_empty() || !key.contains("node_modules/");
            if is_member {
                if key.is_empty() {
                    have_roots = true;
                }
                if !declared.is_empty() || key.is_empty() {
                    decls.push((key.clone(), declared));
                }
                continue;
            }
            let mut deps = Vec::new();
            for field in ["dependencies", "optionalDependencies"] {
                if let Some(m) = json_obj(v, field) {
                    deps.extend(
                        m.iter()
                            .filter_map(|(k, r)| r.as_str().map(|r| (k.clone(), r.to_owned()))),
                    );
                }
            }
            entries.push(Entry {
                key: key.clone(),
                name: str_of(v, "name").map_or_else(|| name_from_key(key), str::to_owned),
                version: str_of(v, "version").unwrap_or("").to_owned(),
                dev: v.get("dev").and_then(Value::as_bool).unwrap_or(false),
                license: str_of(v, "license").map(str::to_owned),
                integrity: str_of(v, "integrity").map(str::to_owned),
                deps,
                link: v.get("link").and_then(Value::as_bool).unwrap_or(false),
            });
        }
    } else if let Some(deps) = json_obj(&doc, "dependencies") {
        flatten_v1("", deps, &mut entries);
    }
    let keys: BTreeMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.key.clone(), i))
        .collect();
    let mut raw = Vec::new();
    for e in &entries {
        if e.link || e.version.is_empty() {
            continue;
        }
        let mut r = Raw::new(&e.name, &e.version);
        r.dev_hint = Some(e.dev);
        r.integrity = e.integrity.clone();
        r.licenses = e.license.iter().cloned().collect();
        for (dn, _range) in &e.deps {
            if let Some(i) = locate(&keys, &e.key, dn) {
                let t = &entries[i];
                if !t.link && !t.version.is_empty() {
                    r.deps.push(RawDep::new(&t.name, Some(&t.version)));
                }
            }
        }
        raw.push(r);
    }
    let mut roots = Vec::new();
    if have_roots {
        for (owner, declared) in &decls {
            for (dn, scope) in declared {
                if let Some(i) = locate(&keys, owner, dn) {
                    let t = &entries[i];
                    if !t.link && !t.version.is_empty() {
                        roots.push(Root::new(&t.name, None, *scope).pinned(&t.version));
                    }
                }
            }
        }
        // Attach the declared ranges from the root entry when available.
        if let Some(root) = doc.get("packages").and_then(|p| p.get("")) {
            for (field, scope) in [
                ("dependencies", Scope::Runtime),
                ("optionalDependencies", Scope::Runtime),
                ("devDependencies", Scope::Development),
            ] {
                if let Some(m) = json_obj(root, field) {
                    for (n, r) in m {
                        if let Some(r) = r.as_str() {
                            for root in roots
                                .iter_mut()
                                .filter(|x| x.name == *n && x.scope == scope)
                            {
                                if root.requirement.is_none() {
                                    root.requirement = Some(r.to_owned());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(LockData {
        raw,
        roots,
        descriptors: BTreeMap::new(),
    })
}

// ---------------------------------------------------------------------------------------------
// pnpm-lock.yaml
// ---------------------------------------------------------------------------------------------

fn strip_peers(v: &str) -> &str {
    v.split('(').next().unwrap_or(v)
}

fn pnpm_split_key(key: &str, old: bool) -> Option<(String, String)> {
    let k = key.strip_prefix('/').unwrap_or(key);
    if old {
        let (name, ver) = k.rsplit_once('/')?;
        let ver = ver.split('_').next().unwrap_or(ver);
        return Some((name.to_owned(), ver.to_owned()));
    }
    let k = strip_peers(k);
    let idx = k.rfind('@').filter(|i| *i > 0)?;
    Some((k[..idx].to_owned(), k[idx + 1..].to_owned()))
}

fn pnpm_dep_version(name: &str, value: &str, old: bool) -> Option<(String, String)> {
    let v = value.trim();
    if v.starts_with("link:") || v.starts_with("file:") || v.starts_with("workspace:") {
        return None;
    }
    if v.starts_with('/') || (v.contains('@') && !v.starts_with(|c: char| c.is_ascii_digit())) {
        return pnpm_split_key(v, old);
    }
    let v = strip_peers(v);
    let v = if old {
        v.split('_').next().unwrap_or(v)
    } else {
        v
    };
    Some((name.to_owned(), v.to_owned()))
}

fn parse_pnpm(path: &str, text: &str) -> Result<LockData> {
    let doc = yaml::parse(path, text)?;
    let version = doc
        .get("lockfileVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let old = version.starts_with('5') || version.starts_with("4");
    let packages = json_obj(&doc, "packages").cloned().unwrap_or_default();
    let snapshots = json_obj(&doc, "snapshots");
    let source = snapshots.unwrap_or(&packages);
    let mut raw: Vec<Raw> = Vec::new();
    for (key, v) in source {
        let Some((name, ver)) = pnpm_split_key(key, old) else {
            continue;
        };
        let mut r = Raw::new(&name, &ver);
        for field in ["dependencies", "optionalDependencies"] {
            if let Some(m) = json_obj(v, field) {
                for (dn, dv) in m {
                    if let Some((n, vv)) = dv.as_str().and_then(|s| pnpm_dep_version(dn, s, old)) {
                        r.deps.push(RawDep::new(&n, Some(&vv)));
                    }
                }
            }
        }
        // `dev` lives on package entries in v5/v6; v9 puts resolution on `packages`.
        let meta = packages
            .get(key)
            .or_else(|| packages.get(strip_peers_key(key)))
            .unwrap_or(v);
        if let Some(d) = meta.get("dev").and_then(Value::as_bool) {
            r.dev_hint = Some(d);
        }
        r.integrity = meta
            .get("resolution")
            .and_then(|x| x.get("integrity"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        raw.push(r);
    }
    let mut roots = Vec::new();
    let mut add_importer = |imp: &Value| {
        for (field, scope) in [
            ("dependencies", Scope::Runtime),
            ("optionalDependencies", Scope::Runtime),
            ("devDependencies", Scope::Development),
        ] {
            let Some(m) = json_obj(imp, field) else {
                continue;
            };
            for (name, v) in m {
                let (spec, ver) = match v {
                    Value::String(s) => (
                        imp.get("specifiers")
                            .and_then(|x| str_of(x, name))
                            .map(str::to_owned),
                        s.clone(),
                    ),
                    other => (
                        str_of(other, "specifier").map(str::to_owned),
                        str_of(other, "version").unwrap_or("").to_owned(),
                    ),
                };
                if let Some((n, vv)) = pnpm_dep_version(name, &ver, old) {
                    roots.push(Root {
                        name: n,
                        requirement: spec,
                        scope,
                        pinned: Some(vv),
                    });
                }
            }
        }
    };
    if let Some(imps) = json_obj(&doc, "importers") {
        for imp in imps.values() {
            add_importer(imp);
        }
    } else {
        add_importer(&doc);
    }
    Ok(LockData {
        raw,
        roots,
        descriptors: BTreeMap::new(),
    })
}

fn strip_peers_key(k: &str) -> &str {
    strip_peers(k)
}

// ---------------------------------------------------------------------------------------------
// yarn.lock
// ---------------------------------------------------------------------------------------------

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
}

fn split_descriptor(spec: &str) -> Option<(&str, &str)> {
    let idx = spec.get(1..)?.find('@')? + 1;
    Some((&spec[..idx], &spec[idx + 1..]))
}

fn parse_yarn_v1(path: &str, text: &str) -> Result<LockData> {
    struct E {
        specs: Vec<String>,
        version: String,
        integrity: Option<String>,
        deps: Vec<(String, String)>,
    }
    let mut entries: Vec<E> = Vec::new();
    let mut in_deps = false;
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let body = line.trim();
        match indent {
            0 => {
                let head = body.strip_suffix(':').ok_or_else(|| {
                    DepsError::invalid(path, format!("line {}: expected an entry header", n + 1))
                })?;
                entries.push(E {
                    specs: head.split(',').map(|s| unquote(s).to_owned()).collect(),
                    version: String::new(),
                    integrity: None,
                    deps: Vec::new(),
                });
                in_deps = false;
            }
            2 => {
                let e = entries
                    .last_mut()
                    .ok_or_else(|| DepsError::invalid(path, "field before entry header"))?;
                in_deps = false;
                if let Some(v) = body.strip_prefix("version ") {
                    e.version = unquote(v).to_owned();
                } else if let Some(v) = body.strip_prefix("integrity ") {
                    e.integrity = Some(unquote(v).to_owned());
                } else if body == "dependencies:" || body == "optionalDependencies:" {
                    in_deps = true;
                }
            }
            _ if in_deps => {
                let e = entries
                    .last_mut()
                    .ok_or_else(|| DepsError::invalid(path, "dependency before entry header"))?;
                let (name, range) = if let Some(rest) = body.strip_prefix('"') {
                    let end = rest.find('"').ok_or_else(|| {
                        DepsError::invalid(path, format!("line {}: bad quoting", n + 1))
                    })?;
                    (&rest[..end], rest[end + 1..].trim())
                } else {
                    body.split_once(' ').unwrap_or((body, ""))
                };
                e.deps.push((name.to_owned(), unquote(range).to_owned()));
            }
            _ => {}
        }
    }
    let mut descriptors: BTreeMap<String, String> = BTreeMap::new();
    for e in &entries {
        for s in &e.specs {
            descriptors.insert(s.clone(), e.version.clone());
        }
    }
    let mut raw = Vec::new();
    for e in &entries {
        let Some(first) = e.specs.first() else {
            continue;
        };
        let Some((name, _)) = split_descriptor(first) else {
            continue;
        };
        if e.version.is_empty() {
            continue;
        }
        let mut r = Raw::new(name, &e.version);
        r.integrity = e.integrity.clone();
        for (dn, range) in &e.deps {
            let v = descriptors.get(&format!("{dn}@{range}"));
            r.deps.push(RawDep::new(
                dn,
                Some(v.map_or(range.as_str(), String::as_str)),
            ));
        }
        raw.push(r);
    }
    Ok(LockData {
        raw,
        roots: Vec::new(),
        descriptors,
    })
}

fn parse_yarn_berry(path: &str, text: &str) -> Result<LockData> {
    let doc = yaml::parse(path, text)?;
    let Some(map) = doc.as_object() else {
        return Err(DepsError::invalid(path, "yarn.lock is not a mapping"));
    };
    let mut descriptors: BTreeMap<String, String> = BTreeMap::new();
    for (key, v) in map {
        if key == "__metadata" {
            continue;
        }
        let version = str_of(v, "version").unwrap_or("");
        for spec in key.split(',') {
            descriptors.insert(unquote(spec).to_owned(), version.to_owned());
        }
    }
    let lookup = |name: &str, range: &str| -> Option<&String> {
        descriptors
            .get(&format!("{name}@npm:{range}"))
            .or_else(|| descriptors.get(&format!("{name}@{range}")))
    };
    let mut raw = Vec::new();
    let mut ws_roots = Vec::new();
    for (key, v) in map {
        if key == "__metadata" {
            continue;
        }
        let resolution = str_of(v, "resolution").unwrap_or("");
        let Some((name, proto_ver)) = split_descriptor(resolution) else {
            continue;
        };
        let version = str_of(v, "version").unwrap_or("");
        let deps_of = |field: &str| -> Vec<(String, String)> {
            json_obj(v, field)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, r)| r.as_str().map(|r| (k.clone(), r.to_owned())))
                        .collect()
                })
                .unwrap_or_default()
        };
        if proto_ver.starts_with("workspace:") {
            for (dn, range) in deps_of("dependencies") {
                let pinned = lookup(&dn, &range).cloned();
                let mut root = Root::new(&dn, Some(&range), Scope::Runtime);
                root.pinned = pinned;
                ws_roots.push(root);
            }
            continue;
        }
        if proto_ver.starts_with("patch:") || version.is_empty() {
            continue;
        }
        let mut r = Raw::new(name, version);
        r.integrity = str_of(v, "checksum").map(str::to_owned);
        for field in ["dependencies"] {
            for (dn, range) in deps_of(field) {
                let ver = lookup(&dn, &range)
                    .map_or(range.as_str(), String::as_str)
                    .to_owned();
                r.deps.push(RawDep::new(&dn, Some(&ver)));
            }
        }
        raw.push(r);
    }
    Ok(LockData {
        raw,
        roots: ws_roots,
        descriptors,
    })
}

// ---------------------------------------------------------------------------------------------

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    let mut manifests: BTreeMap<&str, Vec<Root>> = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::PackageJson) {
        match parse_package_json(i.path, i.content) {
            Ok(r) => {
                manifests.insert(i.path, r);
            }
            Err(e) => out.failed(i, &e),
        }
    }
    let locks: Vec<&In<'_>> = inputs.iter().filter(|i| i.kind.is_lockfile()).collect();
    let lock_dirs: Vec<&str> = locks.iter().map(|l| dir_of(l.path)).collect();
    let mut assigned: BTreeSet<&str> = BTreeSet::new();
    for l in &locks {
        let data = match l.kind {
            Kind::PackageLock => parse_package_lock(l.path, l.content),
            Kind::PnpmLock => parse_pnpm(l.path, l.content),
            _ if l.content.contains("__metadata:") => parse_yarn_berry(l.path, l.content),
            _ => parse_yarn_v1(l.path, l.content),
        };
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                out.failed(l, &e);
                continue;
            }
        };
        let dir = dir_of(l.path);
        let mut manifest_roots: Vec<Root> = Vec::new();
        for (path, roots) in &manifests {
            if within(dir, path) && nearest(&lock_dirs, path) == Some(dir) {
                assigned.insert(path);
                manifest_roots.extend(roots.iter().cloned().map(|mut r| {
                    if let Some(req) = &r.requirement {
                        if let Some(v) = data.descriptors.get(&format!("{}@{req}", r.name)) {
                            r.pinned = Some(v.clone());
                        } else if let Some(v) =
                            data.descriptors.get(&format!("{}@npm:{req}", r.name))
                        {
                            r.pinned = Some(v.clone());
                        }
                    }
                    r
                }));
            }
        }
        let roots = if l.kind != Kind::YarnLock && !data.roots.is_empty() {
            data.roots
        } else if !manifest_roots.is_empty() {
            manifest_roots
        } else {
            data.roots
        };
        let pk = build::build(Ecosystem::Npm, l.path, data.raw, roots, true);
        out.add(l, pk);
    }
    for i in inputs.iter().filter(|i| i.kind == Kind::PackageJson) {
        let Some(roots) = manifests.get(i.path) else {
            continue;
        };
        if assigned.contains(i.path) {
            out.file(i, roots.len());
        } else {
            let pk = build::build(Ecosystem::Npm, i.path, Vec::new(), roots.clone(), false);
            out.add(i, pk);
        }
    }
    out
}
