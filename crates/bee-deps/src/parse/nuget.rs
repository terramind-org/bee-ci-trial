//! NuGet: `packages.lock.json`, project files (`*.csproj`, `*.fsproj`, `*.vbproj`) and
//! central package versions (`Directory.Packages.props`).

use std::collections::BTreeMap;

use serde_json::Value;

use super::build::{self, Raw, RawDep, Root};
use super::{In, Kind, Out, dir_of, within};
use crate::error::{DepsError, Result};
use crate::model::{Ecosystem, Scope};
use crate::xml::{self, Element};

fn parse_lock(path: &str, text: &str) -> Result<(Vec<Raw>, Vec<Root>)> {
    let doc: Value = serde_json::from_str(text)
        .map_err(|e| DepsError::invalid(path, format!("invalid JSON: {e}")))?;
    let tfms = doc
        .get("dependencies")
        .and_then(Value::as_object)
        .ok_or_else(|| DepsError::invalid(path, "missing \"dependencies\" object"))?;
    let mut raw = Vec::new();
    let mut roots = Vec::new();
    for pkgs in tfms.values().filter_map(Value::as_object) {
        for (name, p) in pkgs {
            let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
            let Some(resolved) = p.get("resolved").and_then(Value::as_str) else {
                continue;
            };
            if ty == "Project" {
                continue;
            }
            let mut r = Raw::new(name, resolved);
            r.integrity = p
                .get("contentHash")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(deps) = p.get("dependencies").and_then(Value::as_object) {
                for (dn, range) in deps {
                    let exact = pkgs
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(dn))
                        .and_then(|(_, v)| v.get("resolved"))
                        .and_then(Value::as_str)
                        .or_else(|| range.as_str());
                    r.deps.push(RawDep::new(dn, exact));
                }
            }
            raw.push(r);
            if ty == "Direct" {
                let req = p.get("requested").and_then(Value::as_str);
                roots.push(Root::new(name, req, Scope::Runtime).pinned(resolved));
            }
        }
    }
    Ok((raw, roots))
}

/// `PackageVersion` entries of a `Directory.Packages.props`.
fn central_versions(root: &Element) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    root.walk(&mut |e| {
        if e.local_name() == "PackageVersion"
            && let (Some(n), Some(v)) = (
                e.attr_ci("Include"),
                e.attr_ci("Version").or_else(|| e.child_text("Version")),
            )
        {
            out.insert(n.to_lowercase(), v.to_owned());
        }
    });
    out
}

fn floating_to_min(v: &str) -> String {
    v.split('.')
        .map(|s| if s.contains('*') { "0" } else { s })
        .collect::<Vec<_>>()
        .join(".")
}

fn parse_project(path: &str, root: &Element, central: &BTreeMap<String, String>) -> Vec<Root> {
    let mut props: BTreeMap<String, String> = BTreeMap::new();
    root.walk(&mut |e| {
        if e.local_name() == "PropertyGroup" {
            for c in &e.children {
                props.insert(c.local_name().to_owned(), c.text.clone());
            }
        }
    });
    let mut refs: Vec<(String, Option<String>)> = Vec::new();
    root.walk(&mut |e| {
        if e.local_name() == "PackageReference" {
            let Some(name) = e.attr_ci("Include").or_else(|| e.attr_ci("Update")) else {
                return;
            };
            let ver = e
                .attr_ci("Version")
                .or_else(|| e.child_text("Version"))
                .map(str::to_owned);
            refs.push((name.to_owned(), ver));
        }
    });
    let is_test = props
        .get("IsTestProject")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        || refs
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("Microsoft.NET.Test.Sdk"))
        || path
            .rsplit('/')
            .next()
            .is_some_and(|b| b.contains(".Tests") || b.contains(".Test."));
    let scope = if is_test {
        Scope::Development
    } else {
        Scope::Runtime
    };
    let mut roots = Vec::new();
    for (name, ver) in refs {
        let ver = ver
            .or_else(|| central.get(&name.to_lowercase()).cloned())
            .map(
                |v| match v.strip_prefix("$(").and_then(|x| x.strip_suffix(')')) {
                    Some(key) => props.get(key).cloned().unwrap_or_default(),
                    None => v,
                },
            );
        let mut root = match ver.as_deref().filter(|v| !v.is_empty()) {
            Some(v) if v.contains('*') => {
                Root::new(&name, Some(&floating_to_min(v)), Scope::Runtime)
            }
            Some(v) => {
                let mut r = Root::new(&name, Some(v), Scope::Runtime);
                if !v.starts_with(['[', '(']) {
                    r.pinned = Some(v.to_owned());
                }
                r
            }
            None => Root::new(&name, None, Scope::Runtime),
        };
        root.scope = scope;
        roots.push(root);
    }
    roots
}

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    let mut centrals: Vec<(&str, BTreeMap<String, String>)> = Vec::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::DirectoryPackages) {
        match xml::parse(i.path, i.content) {
            Ok(root) => {
                let v = central_versions(&root);
                out.file(i, v.len());
                centrals.push((i.path, v));
            }
            Err(e) => out.failed(i, &e),
        }
    }
    for i in inputs.iter().filter(|i| i.kind == Kind::NugetLock) {
        match parse_lock(i.path, i.content) {
            Ok((raw, roots)) => {
                let pk = build::build(Ecosystem::NuGet, i.path, raw, roots, true);
                out.add(i, pk);
            }
            Err(e) => out.failed(i, &e),
        }
    }
    let empty = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::Csproj) {
        let root = match xml::parse(i.path, i.content) {
            Ok(r) => r,
            Err(e) => {
                out.failed(i, &e);
                continue;
            }
        };
        let central = centrals
            .iter()
            .filter(|(p, _)| within(dir_of(p), i.path))
            .max_by_key(|(p, _)| dir_of(p).len())
            .map_or(&empty, |(_, m)| m);
        let roots = parse_project(i.path, &root, central);
        let pk = build::build(Ecosystem::NuGet, i.path, Vec::new(), roots, false);
        out.add(i, pk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_references_and_central_versions() {
        let props = xml::parse(
            "Directory.Packages.props",
            "<Project><ItemGroup><PackageVersion Include=\"Serilog\" Version=\"3.1.1\"/></ItemGroup></Project>",
        )
        .unwrap();
        let central = central_versions(&props);
        let proj = xml::parse(
            "App.csproj",
            "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><JV>13.0.1</JV></PropertyGroup><ItemGroup><PackageReference Include=\"Newtonsoft.Json\" Version=\"$(JV)\"/><PackageReference Include=\"Serilog\"/><PackageReference Include=\"Dapper\"><Version>2.*</Version></PackageReference></ItemGroup></Project>",
        )
        .unwrap();
        let roots = parse_project("App.csproj", &proj, &central);
        assert_eq!(roots.len(), 3);
        assert_eq!(roots[0].pinned.as_deref(), Some("13.0.1"));
        assert_eq!(roots[1].pinned.as_deref(), Some("3.1.1"));
        assert_eq!(roots[2].requirement.as_deref(), Some("2.0"));
        assert!(roots[2].pinned.is_none());
    }
}
