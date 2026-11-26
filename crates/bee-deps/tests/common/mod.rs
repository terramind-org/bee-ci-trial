//! Shared helpers for the integration tests.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use bee_deps::{DependencyGraph, Package, Relationship, Scope, SourceFile};

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<SourceFile>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect(root, &p, out);
        } else if let Ok(content) = fs::read_to_string(&p) {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push(SourceFile { path: rel, content });
        }
    }
}

/// Loads every file of `tests/fixtures/repos/<name>` as a revision.
pub fn fixture_files(name: &str) -> Vec<SourceFile> {
    let root = manifest_dir().join("tests/fixtures/repos").join(name);
    let mut out = Vec::new();
    collect(&root, &root, &mut out);
    out
}

pub fn fixture_graph(name: &str) -> DependencyGraph {
    DependencyGraph::from_files(&fixture_files(name))
}

pub fn dump_package(p: &Package) -> String {
    let scope = match p.scope {
        Scope::Runtime => "runtime",
        Scope::Development => "dev",
    };
    let rel = match p.relationship {
        Relationship::Direct => "direct",
        Relationship::Indirect => "indirect",
    };
    let deps: Vec<String> = p
        .dependencies
        .iter()
        .map(|d| d.trim_start_matches("pkg:").to_owned())
        .collect();
    format!(
        "{} {} {}@{} {} {}{} req={} {}{}",
        p.source_path,
        p.ecosystem.proto_name(),
        p.name,
        p.version,
        scope,
        rel,
        if p.resolved { "" } else { " unresolved" },
        p.requirement.as_deref().unwrap_or("-"),
        p.purl,
        if deps.is_empty() {
            String::new()
        } else {
            format!(" -> {}", deps.join(","))
        },
    )
}

pub fn dump(g: &DependencyGraph) -> String {
    let mut s = String::new();
    for m in g.manifests() {
        s.push_str(&format!(
            "manifest {} {} lock={} count={}{}\n",
            m.path,
            m.ecosystem.proto_name(),
            m.is_lockfile,
            m.dependency_count,
            m.error
                .as_ref()
                .map_or(String::new(), |e| format!(" error={e}")),
        ));
    }
    for p in g.packages() {
        s.push_str(&dump_package(p));
        s.push('\n');
    }
    s
}

/// Compares `actual` with `tests/fixtures/golden/<name>.txt`; `BEE_DEPS_BLESS=1` rewrites it.
pub fn assert_golden(name: &str, actual: &str) {
    let path = manifest_dir()
        .join("tests/fixtures/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("BEE_DEPS_BLESS").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden {}; run with BEE_DEPS_BLESS=1",
            path.display()
        )
    });
    assert_eq!(expected, actual, "golden mismatch for {name}");
}

/// Retrieves the vendored schemas that the CycloneDX schema references by relative URI.
struct VendoredSchemas;

impl jsonschema::Retrieve for VendoredSchemas {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let name = uri.as_str().rsplit('/').next().unwrap_or("");
        let path = manifest_dir().join("tests/schemas").join(name);
        Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
    }
}

/// A validator for a vendored schema file (`tests/schemas/<file>`).
pub fn schema_validator(file: &str) -> jsonschema::Validator {
    let text = fs::read_to_string(manifest_dir().join("tests/schemas").join(file)).unwrap();
    let schema: serde_json::Value = serde_json::from_str(&text).unwrap();
    jsonschema::options()
        .with_retriever(VendoredSchemas)
        .build(&schema)
        .unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// Panics listing every schema violation of `doc`.
pub fn assert_valid(v: &jsonschema::Validator, doc: &serde_json::Value, what: &str) {
    let errors: Vec<String> = v
        .iter_errors(doc)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .take(10)
        .collect();
    assert!(
        errors.is_empty(),
        "{what} violates its schema:\n{}",
        errors.join("\n")
    );
}
