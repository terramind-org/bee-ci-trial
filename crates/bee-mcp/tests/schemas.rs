//! Every RPC is a tool with a valid name, description and JSON Schemas.

use crate::common;
use std::collections::BTreeSet;

use bee_mcp::catalog::{ToolCatalog, tool_name};
use bee_proto::methods::{METHODS, MethodKind};
use common::check_schema;
use serde_json::Value;

#[test]
fn one_tool_per_rpc_with_a_valid_name() {
    let catalog = ToolCatalog::global();
    assert_eq!(catalog.tools().len(), METHODS.len());
    let names: BTreeSet<String> = METHODS.iter().map(tool_name).collect();
    assert_eq!(names.len(), METHODS.len(), "two RPCs map to one tool name");
    for tool in catalog.tools() {
        let ok = !tool.name.is_empty()
            && tool.name.len() <= 128
            && tool
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            && !tool.name.contains("__")
            && !tool.name.ends_with('_');
        assert!(ok, "bad tool name {}", tool.name);
        assert!(!tool.description.is_empty());
        assert!(tool.description.contains(tool.method.input_type));
        assert!(!tool.toolsets.is_empty(), "{} is in no toolset", tool.name);
    }
    for (name, path) in [
        ("write_land", "/bee.v1.WriteService/Land"),
        ("change_open_change", "/bee.v1.ChangeService/OpenChange"),
        ("issue_create_issue", "/bee.v1.IssueService/CreateIssue"),
        ("auth_who_am_i", "/bee.v1.AuthService/WhoAmI"),
    ] {
        let tool = catalog.get(name).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(tool.method.path, path);
    }
}

#[test]
fn descriptions_come_from_proto_comments() {
    let login = ToolCatalog::global().get("auth_login").expect("auth_login");
    assert!(
        login.summary().starts_with("Starts or completes"),
        "{}",
        login.description
    );
}

#[test]
fn every_input_and_output_schema_is_valid() {
    for tool in ToolCatalog::global().tools() {
        let input = tool.input_schema();
        check_schema(input).unwrap_or_else(|e| panic!("{} inputSchema: {e}", tool.name));
        let output = tool.output_schema();
        check_schema(output).unwrap_or_else(|e| panic!("{} outputSchema: {e}", tool.name));
        // A JSON object and its MCP rendering agree.
        let rendered = tool.to_json(true, true);
        assert_eq!(&rendered["inputSchema"], input);
        assert_eq!(&rendered["outputSchema"], output);
    }
}

#[test]
fn input_schemas_extend_published_message_schemas_for_repo_refs() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let catalog = ToolCatalog::global();
    for tool in catalog
        .tools()
        .iter()
        .filter(|t| t.method.kind == MethodKind::Unary)
    {
        let path = root.join(format!(
            "schemas/v1/messages/{}.schema.json",
            tool.method.input_type
        ));
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let published: Value = serde_json::from_str(&text).expect("json");
        if published.to_string().contains("bee.v1.RepoRef") {
            assert_ne!(tool.input_schema(), &published, "{}", tool.name);
            let repo_ref = &tool.input_schema()["$defs"]["bee.v1.RepoRef"];
            assert_eq!(repo_ref["anyOf"][0]["type"], "object", "{}", tool.name);
            assert_eq!(repo_ref["anyOf"][1]["type"], "string", "{}", tool.name);
            assert!(
                repo_ref["anyOf"][1]["description"]
                    .as_str()
                    .is_some_and(|text| text.contains("org/name") && text.contains("ULID")),
                "{}",
                tool.name
            );
        } else {
            assert_eq!(tool.input_schema(), &published, "{}", tool.name);
        }
    }
}

#[test]
fn streaming_tools_take_limits_that_never_shadow_request_fields() {
    let catalog = ToolCatalog::global();
    let mut streaming = 0;
    for tool in catalog.tools().iter().filter(|t| t.collects_stream()) {
        streaming += 1;
        let request = bee_mcp::catalog::schema_catalog()
            .message_schema(tool.method.input_type)
            .expect("schema");
        let fields = request["properties"].as_object().expect("properties");
        for added in ["max_items", "timeout_ms", "maxItems", "timeoutMs"] {
            if added.contains('_') {
                assert!(
                    tool.input_schema()["properties"].get(added).is_some(),
                    "{}",
                    tool.name
                );
            }
            assert!(!fields.contains_key(added), "{} has {added}", tool.name);
        }
        assert_eq!(tool.output_schema()["required"][0], "items");
    }
    assert!(streaming > 0);
    let upload = catalog.get("write_upload_objects").expect("upload");
    assert_eq!(upload.input_schema()["required"][0], "messages");
}

#[test]
fn every_tool_is_reachable_through_a_toolset() {
    let catalog = ToolCatalog::global();
    let mut reachable = BTreeSet::new();
    for name in catalog.toolset_names() {
        if name != "all" {
            reachable.extend(catalog.toolset_tools(name).map(|t| t.name.clone()));
        }
    }
    assert_eq!(reachable.len(), catalog.tools().len());
    assert_eq!(catalog.toolset_tools("all").count(), catalog.tools().len());
    let core: Vec<&str> = catalog
        .toolset_tools("core")
        .map(|t| t.method.service)
        .collect();
    assert!(core.contains(&"bee.v1.AuthService"));
    assert_eq!(
        catalog
            .toolset_tools("core")
            .filter(|t| t.method.service == "bee.v1.AuthService")
            .count(),
        1
    );
}

#[test]
fn brief_tools_are_in_the_core_and_changes_toolsets() {
    let catalog = ToolCatalog::global();
    let names = [
        "brief_set_change_brief",
        "brief_get_brief_checklist",
        "brief_add_evidence",
        "brief_begin_evidence_upload",
        "brief_finalize_evidence",
        "brief_get_evidence",
        "brief_list_evidence",
        "brief_update_evidence",
        "brief_delete_evidence",
    ];
    for set in ["core", "changes"] {
        let tools: BTreeSet<&str> = catalog
            .toolset_tools(set)
            .map(|t| t.name.as_str())
            .collect();
        for name in names {
            assert!(tools.contains(name), "{name} is missing from {set}");
        }
    }
    // The checklist is a read, so a client may call it without asking; evidence deletion is not.
    let checklist = catalog
        .get("brief_get_brief_checklist")
        .unwrap()
        .to_json(true, false);
    assert_eq!(checklist["annotations"]["readOnlyHint"], true);
    let delete = catalog
        .get("brief_delete_evidence")
        .unwrap()
        .to_json(true, false);
    assert_eq!(delete["annotations"]["destructiveHint"], true);
}

#[test]
fn annotations_mark_reads_and_deletes() {
    let catalog = ToolCatalog::global();
    let log = catalog.get("query_log").unwrap().to_json(true, false);
    assert_eq!(log["annotations"]["readOnlyHint"], true);
    let delete = catalog.get("ref_delete_ref").unwrap().to_json(true, false);
    assert_eq!(delete["annotations"]["readOnlyHint"], false);
    assert_eq!(delete["annotations"]["destructiveHint"], true);
    let land = catalog.get("write_land").unwrap().to_json(true, false);
    assert_eq!(land["annotations"]["destructiveHint"], false);
    assert!(land.get("outputSchema").is_none());
}
