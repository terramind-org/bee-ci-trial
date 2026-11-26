//! The CLI and MCP re-print the server's JSON, so parsing it must keep every number exact.
//!
//! serde_json parses floats lossily (the last digit can change) unless `float_roundtrip` is
//! on. Test builds get the feature from dev-dependencies (jsonschema), so a standalone
//! `cargo build -p bee-cli` printed `13.27433628318584` where the server sent
//! `13.274336283185841` (found by the three-form conformance suite with the real binary).
//! The manifests must therefore ask for the feature themselves.

use std::path::Path;

fn serde_json_line(manifest: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(manifest);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .find(|l| l.trim_start().starts_with("serde_json"))
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn cli_and_mcp_parse_json_numbers_exactly() {
    for manifest in ["Cargo.toml", "../bee-mcp/Cargo.toml"] {
        let line = serde_json_line(manifest);
        assert!(
            line.contains("float_roundtrip"),
            "{manifest}: serde_json needs the float_roundtrip feature: {line}"
        );
    }
    let text = r#"{"percentage":13.274336283185841}"#;
    let value: serde_json::Value = serde_json::from_str(text).expect("json");
    assert_eq!(value.to_string(), text);
}
