//! Maven and Gradle: `pom.xml`, `gradle.lockfile` and version catalogs (`libs.versions.toml`).
//!
//! `pom.xml` resolution is basic: declared dependencies with versions from the pom, its
//! `dependencyManagement` and `<properties>`, inherited through parent poms that are in the same
//! revision. Transitive dependencies and imported BOMs are not resolved.

use std::collections::{BTreeMap, BTreeSet};

use toml::Value;

use super::build::{self, Raw, Root};
use super::{In, Kind, Out};
use crate::error::{DepsError, Result};
use crate::model::{Ecosystem, Scope};
use crate::xml::{self, Element};

#[derive(Clone, Debug)]
struct PomDep {
    group: String,
    artifact: String,
    version: Option<String>,
    scope: Option<String>,
}

struct Pom {
    group: Option<String>,
    artifact: String,
    version: Option<String>,
    parent: Option<(String, String)>,
    parent_version: Option<String>,
    props: BTreeMap<String, String>,
    managed: BTreeMap<String, String>,
    deps: Vec<PomDep>,
}

fn pom_deps(el: Option<&Element>) -> Vec<PomDep> {
    el.map(|d| {
        d.children_named("dependency")
            .filter_map(|x| {
                Some(PomDep {
                    group: x.child_text("groupId")?.to_owned(),
                    artifact: x.child_text("artifactId")?.to_owned(),
                    version: x.child_text("version").map(str::to_owned),
                    scope: x.child_text("scope").map(str::to_owned),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

fn parse_pom(path: &str, text: &str) -> Result<Pom> {
    let root = xml::parse(path, text)?;
    if root.local_name() != "project" {
        return Err(DepsError::invalid(path, "root element is not <project>"));
    }
    let parent = root.child("parent");
    let mut props = BTreeMap::new();
    if let Some(p) = root.child("properties") {
        for c in &p.children {
            props.insert(c.local_name().to_owned(), c.text.clone());
        }
    }
    let mut managed = BTreeMap::new();
    for d in pom_deps(
        root.child("dependencyManagement")
            .and_then(|m| m.child("dependencies")),
    ) {
        if d.scope.as_deref() == Some("import") {
            continue;
        }
        if let Some(v) = d.version {
            managed.insert(format!("{}:{}", d.group, d.artifact), v);
        }
    }
    Ok(Pom {
        group: root.child_text("groupId").map(str::to_owned),
        artifact: root.child_text("artifactId").unwrap_or("").to_owned(),
        version: root.child_text("version").map(str::to_owned),
        parent: parent.and_then(|p| {
            Some((
                p.child_text("groupId")?.to_owned(),
                p.child_text("artifactId")?.to_owned(),
            ))
        }),
        parent_version: parent
            .and_then(|p| p.child_text("version"))
            .map(str::to_owned),
        props,
        managed,
        deps: pom_deps(root.child("dependencies")),
    })
}

fn interpolate(s: &str, props: &BTreeMap<String, String>) -> String {
    let mut cur = s.to_owned();
    for _ in 0..10 {
        let Some(start) = cur.find("${") else { break };
        let Some(len) = cur[start..].find('}') else {
            break;
        };
        let key = cur[start + 2..start + len].to_owned();
        let Some(val) = props.get(&key) else { break };
        cur.replace_range(start..=start + len, val);
    }
    cur
}

fn is_plain_version(v: &str) -> bool {
    !v.is_empty() && !v.contains(['[', '(', ',', '$'])
}

fn parse_poms(inputs: &[In<'_>], out: &mut Out) {
    let mut poms: BTreeMap<&str, Pom> = BTreeMap::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::PomXml) {
        match parse_pom(i.path, i.content) {
            Ok(p) => {
                poms.insert(i.path, p);
            }
            Err(e) => out.failed(i, &e),
        }
    }
    // Coordinates of every pom, for parent lookup.
    let coords: BTreeMap<(String, String), &str> = poms
        .iter()
        .filter_map(|(path, p)| {
            let g = p
                .group
                .clone()
                .or_else(|| p.parent.as_ref().map(|x| x.0.clone()))?;
            Some(((g, p.artifact.clone()), *path))
        })
        .collect();
    for i in inputs.iter().filter(|i| i.kind == Kind::PomXml) {
        let Some(pom) = poms.get(i.path) else {
            continue;
        };
        // Walk the parent chain, child first.
        let mut chain: Vec<&Pom> = vec![pom];
        let mut seen: BTreeSet<&str> = BTreeSet::from([i.path]);
        let mut cur = pom;
        while let Some(pc) = &cur.parent {
            let Some(path) = coords.get(pc) else { break };
            if !seen.insert(path) {
                break;
            }
            cur = &poms[path];
            chain.push(cur);
        }
        let mut props: BTreeMap<String, String> = BTreeMap::new();
        let mut managed: BTreeMap<String, String> = BTreeMap::new();
        for p in chain.iter().rev() {
            props.extend(p.props.clone());
            managed.extend(p.managed.clone());
        }
        let group = pom
            .group
            .clone()
            .or_else(|| pom.parent.as_ref().map(|x| x.0.clone()))
            .unwrap_or_default();
        let version = pom
            .version
            .clone()
            .or_else(|| pom.parent_version.clone())
            .unwrap_or_default();
        for (k, v) in [
            ("project.groupId", &group),
            ("pom.groupId", &group),
            ("project.version", &version),
            ("pom.version", &version),
            ("version", &version),
            ("project.artifactId", &pom.artifact),
        ] {
            props.entry(k.to_owned()).or_insert_with(|| v.clone());
        }
        if let Some(pv) = &pom.parent_version {
            props
                .entry("project.parent.version".to_owned())
                .or_insert_with(|| pv.clone());
        }
        let mut roots = Vec::new();
        for d in &pom.deps {
            let g = interpolate(&d.group, &props);
            let a = interpolate(&d.artifact, &props);
            let name = format!("{g}:{a}");
            let ver = d
                .version
                .clone()
                .or_else(|| managed.get(&name).cloned())
                .map(|v| interpolate(&v, &props));
            let scope = match d.scope.as_deref() {
                Some("test") => Scope::Development,
                _ => Scope::Runtime,
            };
            let mut root = Root::new(&name, ver.as_deref(), scope);
            if let Some(v) = ver.as_deref().filter(|v| is_plain_version(v)) {
                root.pinned = Some(v.to_owned());
            }
            roots.push(root);
        }
        let pk = build::build(Ecosystem::Maven, i.path, Vec::new(), roots, false);
        out.add(i, pk);
    }
}

/// A library entry of a version catalog: `(name, version)`.
type CatalogLib = (String, Option<String>);

/// The library entries of a version catalog.
fn parse_catalog(path: &str, text: &str) -> Result<Vec<CatalogLib>> {
    let doc: Value =
        toml::from_str(text).map_err(|e| DepsError::invalid(path, format!("invalid TOML: {e}")))?;
    let versions: BTreeMap<String, String> = doc
        .get("versions")
        .and_then(Value::as_table)
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| Some((k.clone(), version_text(v)?)))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    if let Some(libs) = doc.get("libraries").and_then(Value::as_table) {
        for v in libs.values() {
            match v {
                Value::String(s) => {
                    let parts: Vec<&str> = s.split(':').collect();
                    if parts.len() >= 2 {
                        out.push((
                            format!("{}:{}", parts[0], parts[1]),
                            parts.get(2).map(|x| (*x).to_owned()),
                        ));
                    }
                }
                Value::Table(t) => {
                    let name = match t.get("module").and_then(Value::as_str) {
                        Some(m) => m.to_owned(),
                        None => match (
                            t.get("group").and_then(Value::as_str),
                            t.get("name").and_then(Value::as_str),
                        ) {
                            (Some(g), Some(n)) => format!("{g}:{n}"),
                            _ => continue,
                        },
                    };
                    let version = t.get("version").and_then(|ver| match ver {
                        Value::Table(r) => match r.get("ref").and_then(Value::as_str) {
                            Some(k) => versions.get(k).cloned(),
                            None => version_text(ver),
                        },
                        other => version_text(other),
                    });
                    out.push((name, version));
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

fn version_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Table(t) => ["strictly", "require", "prefer"]
            .iter()
            .find_map(|k| t.get(*k).and_then(Value::as_str))
            .map(str::to_owned),
        _ => None,
    }
}

fn parse_gradle_lock(text: &str) -> Vec<Raw> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with("empty=") {
            continue;
        }
        let Some((coord, confs)) = l.split_once('=') else {
            continue;
        };
        let parts: Vec<&str> = coord.split(':').collect();
        if parts.len() < 3 {
            continue;
        }
        let mut r = Raw::new(&format!("{}:{}", parts[0], parts[1]), parts[2]);
        let confs: Vec<&str> = confs.split(',').collect();
        r.dev_hint = Some(
            !confs.is_empty()
                && confs
                    .iter()
                    .all(|c| c.to_ascii_lowercase().contains("test")),
        );
        out.push(r);
    }
    out
}

pub(crate) fn parse(inputs: &[In<'_>]) -> Out {
    let mut out = Out::default();
    parse_poms(inputs, &mut out);
    let mut catalog_libs: Vec<(String, Option<String>)> = Vec::new();
    let mut catalogs: Vec<(&In<'_>, Vec<CatalogLib>)> = Vec::new();
    for i in inputs.iter().filter(|i| i.kind == Kind::VersionCatalog) {
        match parse_catalog(i.path, i.content) {
            Ok(libs) => {
                catalog_libs.extend(libs.iter().cloned());
                catalogs.push((i, libs));
            }
            Err(e) => out.failed(i, &e),
        }
    }
    let locks: Vec<&In<'_>> = inputs
        .iter()
        .filter(|i| i.kind == Kind::GradleLock)
        .collect();
    for l in &locks {
        let raw = parse_gradle_lock(l.content);
        let roots: Vec<Root> = catalog_libs
            .iter()
            .map(|(n, v)| {
                // A catalog library only used by test configurations is a development dependency.
                let mut hints = raw.iter().filter(|r| &r.name == n).map(|r| r.dev_hint);
                let dev =
                    hints.next().is_some_and(|h| h == Some(true)) && hints.all(|h| h == Some(true));
                Root::new(
                    n,
                    v.as_deref(),
                    if dev {
                        Scope::Development
                    } else {
                        Scope::Runtime
                    },
                )
            })
            .collect();
        let pk = build::build(Ecosystem::Maven, l.path, raw, roots, true);
        out.add(l, pk);
    }
    for (i, libs) in catalogs {
        if locks.is_empty() {
            let roots = libs
                .iter()
                .map(|(n, v)| {
                    let mut r = Root::new(n, v.as_deref(), Scope::Runtime);
                    if let Some(v) = v.as_deref().filter(|v| is_plain_version(v)) {
                        r.pinned = Some(v.to_owned());
                    }
                    r
                })
                .collect();
            let pk = build::build(Ecosystem::Maven, i.path, Vec::new(), roots, false);
            out.add(i, pk);
        } else {
            out.file(i, libs.len());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation() {
        let mut p = BTreeMap::new();
        p.insert("a".to_owned(), "${b}".to_owned());
        p.insert("b".to_owned(), "1.2".to_owned());
        assert_eq!(interpolate("v${a}-x", &p), "v1.2-x");
        assert_eq!(interpolate("${missing}", &p), "${missing}");
    }

    #[test]
    fn catalog_forms() {
        let libs = parse_catalog(
            "libs.versions.toml",
            "[versions]\nk = \"1.5\"\nr = { strictly = \"2.0\" }\n[libraries]\na = \"g:a:1.0\"\nb = { module = \"g:b\", version.ref = \"k\" }\nc = { group = \"g\", name = \"c\", version = \"3\" }\nd = { module = \"g:d\" }\ne = { module = \"g:e\", version.ref = \"r\" }\n",
        )
        .unwrap();
        assert_eq!(libs.len(), 5);
        assert_eq!(libs[1], ("g:b".to_owned(), Some("1.5".to_owned())));
        assert_eq!(libs[3], ("g:d".to_owned(), None));
        assert_eq!(libs[4].1.as_deref(), Some("2.0"));
    }

    #[test]
    fn gradle_lock_lines() {
        let r = parse_gradle_lock(
            "# comment\norg.x:y:1.0=compileClasspath,runtimeClasspath\norg.t:u:2.0=testCompileClasspath\nempty=annotationProcessor\n",
        );
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].dev_hint, Some(false));
        assert_eq!(r[1].dev_hint, Some(true));
    }
}
