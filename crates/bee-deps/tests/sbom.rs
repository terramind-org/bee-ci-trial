//! SBOM export tests: documents validate against the vendored published JSON schemas
//! (SPDX 2.3 and CycloneDX 1.6) and are deterministic.

use crate::common;
use bee_deps::DependencyGraph;
use bee_deps::sbom::{SbomOptions, cyclonedx_json, spdx_json, to_json_string};
use common::{assert_valid, fixture_files, schema_validator};
use serde_json::Value;

fn opts() -> SbomOptions {
    SbomOptions::new("acme/shop", "2026-03-04T05:06:07Z")
}

/// A revision with many ecosystems at once.
fn big_graph() -> DependencyGraph {
    let mut files = Vec::new();
    for (dir, name) in [
        ("rust", "cargo"),
        ("web", "npm-v3"),
        ("py", "poetry"),
        ("go", "go"),
        ("ruby", "ruby"),
        ("java", "maven"),
        ("dotnet", "nuget"),
        ("php", "composer"),
        ("ci", "actions"),
        ("pnpm", "pnpm-v9"),
    ] {
        for mut f in fixture_files(name) {
            f.path = format!("{dir}/{}", f.path);
            files.push(f);
        }
    }
    DependencyGraph::from_files(&files)
}

#[test]
fn spdx_document_validates_against_the_2_3_schema() {
    let g = big_graph();
    assert!(g.packages().len() > 40);
    let doc = spdx_json(&g, &opts());
    assert_valid(
        &schema_validator("spdx-2.3.schema.json"),
        &doc,
        "SPDX document",
    );
    assert_eq!(doc["spdxVersion"], "SPDX-2.3");
    assert_eq!(doc["SPDXID"], "SPDXRef-DOCUMENT");
}

#[test]
fn cyclonedx_document_validates_against_the_1_6_schema() {
    let g = big_graph();
    let doc = cyclonedx_json(&g, &opts());
    assert_valid(
        &schema_validator("bom-1.6.schema.json"),
        &doc,
        "CycloneDX document",
    );
    assert_eq!(doc["specVersion"], "1.6");
    assert_eq!(doc["bomFormat"], "CycloneDX");
}

#[test]
fn empty_graph_still_validates() {
    let g = DependencyGraph::default();
    assert_valid(
        &schema_validator("spdx-2.3.schema.json"),
        &spdx_json(&g, &opts()),
        "empty SPDX",
    );
    assert_valid(
        &schema_validator("bom-1.6.schema.json"),
        &cyclonedx_json(&g, &opts()),
        "empty CycloneDX",
    );
}

#[test]
fn output_is_deterministic_and_order_independent() {
    let g1 = big_graph();
    let mut files = Vec::new();
    for name in ["cargo", "npm-v3", "go"] {
        files.extend(fixture_files(name));
    }
    let a = DependencyGraph::from_files(&files);
    files.reverse();
    let b = DependencyGraph::from_files(&files);
    for f in [spdx_json, cyclonedx_json] {
        assert_eq!(
            to_json_string(&f(&a, &opts())),
            to_json_string(&f(&b, &opts()))
        );
        assert_eq!(
            to_json_string(&f(&g1, &opts())),
            to_json_string(&f(&g1, &opts()))
        );
    }
    // A different component set changes the namespace and serial number.
    assert_ne!(
        spdx_json(&a, &opts())["documentNamespace"],
        spdx_json(&g1, &opts())["documentNamespace"]
    );
    assert_ne!(
        cyclonedx_json(&a, &opts())["serialNumber"],
        cyclonedx_json(&g1, &opts())["serialNumber"]
    );
}

#[test]
fn spdx_relationships_describe_the_dependency_graph() {
    let g = DependencyGraph::from_files(&fixture_files("cargo"));
    let doc = spdx_json(&g, &opts());
    let pkgs = doc["packages"].as_array().unwrap();
    let id_of = |name: &str| -> String {
        pkgs.iter().find(|p| p["name"] == name).unwrap()["SPDXID"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let rels = doc["relationships"].as_array().unwrap();
    let has = |a: &str, t: &str, b: &str| {
        rels.iter().any(|r| {
            r["spdxElementId"] == a && r["relationshipType"] == t && r["relatedSpdxElement"] == b
        })
    };
    assert!(has("SPDXRef-DOCUMENT", "DESCRIBES", "SPDXRef-Root"));
    assert!(has("SPDXRef-Root", "DEPENDS_ON", &id_of("regex")));
    assert!(has(&id_of("regex"), "DEPENDS_ON", &id_of("aho-corasick")));
    assert!(has(&id_of("tempfile"), "DEV_DEPENDENCY_OF", "SPDXRef-Root"));
    let regex = pkgs.iter().find(|p| p["name"] == "regex").unwrap();
    assert_eq!(
        regex["externalRefs"][0]["referenceLocator"],
        "pkg:cargo/regex@1.10.4"
    );
    assert_eq!(regex["checksums"][0]["algorithm"], "SHA256");
}

#[test]
fn cyclonedx_components_carry_purl_scope_licenses_and_edges() {
    let g = DependencyGraph::from_files(&fixture_files("npm-v3"));
    let doc = cyclonedx_json(&g, &opts());
    let comps = doc["components"].as_array().unwrap();
    let express = comps.iter().find(|c| c["name"] == "express").unwrap();
    assert_eq!(express["purl"], "pkg:npm/express@4.17.1");
    assert_eq!(express["bom-ref"], "pkg:npm/express@4.17.1");
    assert_eq!(express["scope"], "required");
    assert_eq!(express["licenses"][0]["expression"], "MIT");
    assert_eq!(express["hashes"][0]["alg"], "SHA-512");
    assert_eq!(express["hashes"][0]["content"].as_str().unwrap().len(), 128);
    let jest = comps.iter().find(|c| c["name"] == "jest").unwrap();
    assert_eq!(jest["scope"], "optional");
    let deps = doc["dependencies"].as_array().unwrap();
    let root = deps.iter().find(|d| d["ref"] == "root").unwrap();
    let root_deps: Vec<&str> = root["dependsOn"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(root_deps.contains(&"pkg:npm/express@4.17.1"));
    assert!(!root_deps.contains(&"pkg:npm/accepts@1.3.8"));
    let express_deps = deps
        .iter()
        .find(|d| d["ref"] == "pkg:npm/express@4.17.1")
        .unwrap();
    assert_eq!(express_deps["dependsOn"][0], "pkg:npm/accepts@1.3.8");
}

#[test]
fn documents_round_trip_through_json() {
    let g = big_graph();
    for doc in [spdx_json(&g, &opts()), cyclonedx_json(&g, &opts())] {
        let text = to_json_string(&doc);
        assert!(text.ends_with('\n'));
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back, doc);
    }
}

#[test]
fn the_schema_checks_actually_reject_broken_documents() {
    let g = DependencyGraph::from_files(&fixture_files("cargo"));
    let mut spdx = spdx_json(&g, &opts());
    spdx.as_object_mut().unwrap().remove("spdxVersion");
    assert!(!schema_validator("spdx-2.3.schema.json").is_valid(&spdx));
    let mut cdx = cyclonedx_json(&g, &opts());
    cdx["components"][0].as_object_mut().unwrap().remove("name");
    assert!(!schema_validator("bom-1.6.schema.json").is_valid(&cdx));
    let mut bad_serial = cyclonedx_json(&g, &opts());
    bad_serial["serialNumber"] = Value::String("not-a-uuid".into());
    assert!(!schema_validator("bom-1.6.schema.json").is_valid(&bad_serial));
}
