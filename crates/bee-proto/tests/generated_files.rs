//! The checked-in OpenAPI document and JSON Schemas equal a fresh generation.
//!
//! Regenerate with `cargo run -p bee-proto --bin bee-proto-gen`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use bee_proto::methods::METHODS;
use bee_proto::schema::{Catalog, MESSAGES_DIR, OPENAPI_PATH};
use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn generated_files_are_up_to_date() {
    let files = Catalog::load().generated_files();
    let mut stale = Vec::new();
    for (relative, content) in &files {
        let on_disk = std::fs::read_to_string(root().join(relative)).unwrap_or_default();
        if &on_disk != content {
            stale.push(relative.clone());
        }
    }
    let on_disk: BTreeSet<String> = std::fs::read_dir(root().join(MESSAGES_DIR))
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| format!("{MESSAGES_DIR}/{}", e.file_name().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default();
    let extra: Vec<&String> = on_disk.iter().filter(|p| !files.contains_key(*p)).collect();
    assert!(
        stale.is_empty() && extra.is_empty(),
        "generated files are stale ({} differ, {} extra, e.g. {:?} {:?}); run \
         `cargo run -p bee-proto --bin bee-proto-gen`",
        stale.len(),
        extra.len(),
        stale.first(),
        extra.first()
    );
}

#[test]
fn openapi_covers_every_rpc() {
    let openapi = Catalog::load().openapi();
    assert_eq!(openapi["openapi"], "3.1.0");
    let paths = openapi["paths"].as_object().expect("paths");
    assert_eq!(paths.len(), METHODS.len());
    let schemas = openapi["components"]["schemas"]
        .as_object()
        .expect("schemas");
    for method in METHODS {
        let operation = &paths[method.path]["post"];
        let content = if method.kind.is_unary() {
            "application/json"
        } else {
            "application/connect+json"
        };
        let input = operation["requestBody"]["content"][content]["schema"]["$ref"]
            .as_str()
            .expect("request ref");
        assert_eq!(input, format!("#/components/schemas/{}", method.input_type));
        assert!(schemas.contains_key(method.input_type));
        assert!(schemas.contains_key(method.output_type));
        assert_eq!(
            operation["responses"]["default"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/connect.Error"
        );
    }
    // Every $ref resolves.
    let text = serde_json::to_string(&openapi).expect("json");
    for part in text.split("\"$ref\":\"#/components/schemas/").skip(1) {
        let name = part.split('"').next().expect("name");
        assert!(schemas.contains_key(name), "dangling $ref {name}");
    }
}

#[test]
fn message_schemas_are_self_contained() {
    let catalog = Catalog::load();
    let schema = catalog
        .message_schema("bee.v1.LandRequest")
        .expect("schema");
    assert_eq!(schema["type"], "object");
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["properties"]["expectedVersion"]["type"][0], "string");
    assert_eq!(
        schema["properties"]["repo"]["$ref"],
        "#/$defs/bee.v1.RepoRef"
    );
    assert_eq!(
        schema["$defs"]["bee.v1.LandMethod"]["enum"][2],
        "LAND_METHOD_SQUASH"
    );
    for (relative, content) in catalog.generated_files() {
        if relative == OPENAPI_PATH {
            continue;
        }
        let value: Value = serde_json::from_str(&content).expect("json");
        let defs = value["$defs"].as_object().cloned().unwrap_or_default();
        for part in content.split("\"$ref\": \"#/$defs/").skip(1) {
            let name = part.split('"').next().expect("name");
            assert!(defs.contains_key(name), "{relative}: dangling $ref {name}");
        }
    }
}
