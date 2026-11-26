//! MCP protocol conformance over the stdio transport: lifecycle, JSON-RPC errors, pagination,
//! meta-tools and `notifications/tools/list_changed`.

use crate::common;
use std::collections::BTreeSet;

use bee_mcp::catalog::ToolCatalog;
use bee_mcp::protocol::{
    INVALID_PARAMS, INVALID_REQUEST, LATEST_PROTOCOL_VERSION, METHOD_NOT_FOUND, PARSE_ERROR,
};
use bee_mcp::{ClientConfig, McpConfig, McpServer};
use common::StdioClient;
use serde_json::{Value, json};

fn offline_config() -> McpConfig {
    McpConfig::new(ClientConfig::new("http://127.0.0.1:9"))
}

fn client() -> StdioClient {
    StdioClient::start(McpServer::new(offline_config()).expect("config"))
}

async fn list_all(client: &mut StdioClient, first_id: i64) -> Vec<Value> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut id = first_id;
    loop {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let response = client.request(id, "tools/list", params).await;
        id += 1;
        let result = &response["result"];
        tools.extend(result["tools"].as_array().expect("tools").iter().cloned());
        match result.get("nextCursor").and_then(Value::as_str) {
            Some(next) => cursor = Some(next.to_owned()),
            None => return tools,
        }
    }
}

fn names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_owned())
        .collect()
}

#[tokio::test]
async fn initialize_negotiates_the_protocol_version() {
    let mut c = client();
    let response = c.initialize("2025-06-18").await;
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
    assert_eq!(result["serverInfo"]["name"], "bee-mcp");
    assert!(
        result["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("search_tools"))
    );
    let again = c
        .request(1, "initialize", json!({"protocolVersion": "2025-06-18"}))
        .await;
    assert_eq!(again["error"]["code"], INVALID_REQUEST);
    c.close().await;

    let mut c = client();
    let response = c.initialize("1999-01-01").await;
    assert_eq!(
        response["result"]["protocolVersion"],
        LATEST_PROTOCOL_VERSION
    );
    c.close().await;
}

#[tokio::test]
async fn only_initialize_and_ping_precede_initialization() {
    let mut c = client();
    let ping = c.request(1, "ping", json!({})).await;
    assert_eq!(ping["result"], json!({}));
    let early = c.request(2, "tools/list", json!({})).await;
    assert_eq!(early["error"]["code"], INVALID_REQUEST);
    let missing = c.request(3, "initialize", json!({})).await;
    assert_eq!(missing["error"]["code"], INVALID_PARAMS);
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let listed = c.request(4, "tools/list", json!({})).await;
    assert!(listed["result"]["tools"].is_array());
    c.close().await;
}

#[tokio::test]
async fn json_rpc_errors() {
    let mut c = client();
    c.send_raw("{not json").await;
    let parse = c.recv().await;
    assert_eq!(parse["error"]["code"], PARSE_ERROR);
    assert_eq!(parse["id"], Value::Null);

    c.send(&json!({"jsonrpc": "2.0", "id": 5})).await;
    let invalid = c.recv().await;
    assert_eq!(invalid["error"]["code"], INVALID_REQUEST);
    assert_eq!(invalid["id"], 5);

    c.send(&json!([])).await;
    assert_eq!(c.recv().await["error"]["code"], INVALID_REQUEST);

    c.initialize(LATEST_PROTOCOL_VERSION).await;
    for (id, method) in [
        (6, "resources/list"),
        (7, "server/discover"),
        (8, "prompts/get"),
    ] {
        let response = c.request(id, method, json!({})).await;
        assert_eq!(response["error"]["code"], METHOD_NOT_FOUND, "{method}");
    }
    let unknown_tool = c
        .request(
            9,
            "tools/call",
            json!({"name": "no_such_tool", "arguments": {}}),
        )
        .await;
    assert_eq!(unknown_tool["error"]["code"], INVALID_PARAMS);
    let bad_args = c
        .request(
            10,
            "tools/call",
            json!({"name": "query_log", "arguments": [1]}),
        )
        .await;
    assert_eq!(bad_args["error"]["code"], INVALID_PARAMS);
    let bad_cursor = c.request(11, "tools/list", json!({"cursor": "zzz"})).await;
    assert_eq!(bad_cursor["error"]["code"], INVALID_PARAMS);
    // Unknown notifications are ignored, string ids are echoed.
    c.notify("notifications/whatever", json!({})).await;
    c.send(&json!({"jsonrpc": "2.0", "id": "s-1", "method": "ping"}))
        .await;
    assert_eq!(c.recv().await["id"], "s-1");
    c.close().await;
}

#[tokio::test]
async fn batches_are_answered_as_arrays() {
    let mut c = client();
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    c.send(&json!([
        {"jsonrpc": "2.0", "id": 1, "method": "ping"},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "nope"}
    ]))
    .await;
    let reply = c.recv().await;
    let items = reply.as_array().expect("array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["result"], json!({}));
    assert_eq!(items[1]["error"]["code"], METHOD_NOT_FOUND);
    c.close().await;
}

#[tokio::test]
async fn tools_list_paginates_the_default_toolset() {
    let mut config = offline_config();
    config.page_size = 17;
    let mut c = StdioClient::start(McpServer::new(config).expect("config"));
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let tools = list_all(&mut c, 1).await;
    let listed = names(&tools);
    let unique: BTreeSet<&String> = listed.iter().collect();
    assert_eq!(unique.len(), listed.len(), "duplicates across pages");
    assert_eq!(
        &listed[..3],
        ["search_tools", "list_toolsets", "enable_toolset"]
    );

    let catalog = ToolCatalog::global();
    let core: Vec<&str> = catalog
        .toolset_tools("core")
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(listed.len(), core.len() + 3);
    assert!(listed.len() > 17, "the test must span pages");
    for name in [
        "write_land",
        "query_log",
        "ref_list_refs",
        "auth_who_am_i",
        "lease_acquire_lease",
    ] {
        assert!(listed.iter().any(|n| n == name), "{name} missing");
    }
    assert!(!listed.iter().any(|n| n == "auth_login"));
    assert!(!listed.iter().any(|n| n.starts_with("workflow_")));
    for tool in &tools {
        assert!(tool["inputSchema"]["type"] == "object", "{}", tool["name"]);
        assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()));
    }
    c.close().await;
}

#[tokio::test]
async fn enable_toolset_emits_list_changed() {
    let mut c = client();
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let before = list_all(&mut c, 1).await.len();
    assert!(c.notifications.is_empty());

    let result = c
        .call(100, "enable_toolset", json!({"toolsets": ["ci"]}))
        .await;
    assert_eq!(result["isError"], false);
    assert_eq!(
        c.notifications,
        vec![json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})]
    );
    let added = result["structuredContent"]["added"]
        .as_u64()
        .expect("added");
    assert!(added > 0);
    let after = names(&list_all(&mut c, 200).await);
    assert_eq!(after.len(), before + added as usize);
    assert!(after.iter().any(|n| n.starts_with("workflow_")));

    // Enabling what is already listed changes nothing and notifies nobody.
    c.notifications.clear();
    let again = c
        .call(300, "enable_toolset", json!({"toolset": "ci"}))
        .await;
    assert_eq!(again["structuredContent"]["added"], 0);
    assert!(c.notifications.is_empty());

    // Single tools can be listed too.
    let one = c
        .call(301, "enable_toolset", json!({"tools": ["auth_login"]}))
        .await;
    assert_eq!(one["structuredContent"]["added"], 1);
    assert_eq!(c.notifications.len(), 1);

    let unknown = c
        .call(302, "enable_toolset", json!({"toolsets": ["nope"]}))
        .await;
    assert_eq!(unknown["isError"], true);
    assert_eq!(unknown["structuredContent"]["reason"], "NOT_FOUND");
    let empty = c.call(303, "enable_toolset", json!({})).await;
    assert_eq!(empty["structuredContent"]["reason"], "INVALID_ARGUMENT");
    c.close().await;
}

#[tokio::test]
async fn search_and_list_toolsets_reach_the_whole_catalog() {
    let mut c = client();
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let found = c
        .call(
            1,
            "search_tools",
            json!({"query": "create issue", "limit": 5}),
        )
        .await;
    let matches = found["structuredContent"]["matches"]
        .as_array()
        .expect("matches");
    assert!(matches.len() <= 5);
    assert_eq!(matches[0]["name"], "issue_create_issue");
    assert_eq!(matches[0]["listed"], true);

    let found = c
        .call(2, "search_tools", json!({"query": "workflow runs"}))
        .await;
    let first = &found["structuredContent"]["matches"][0];
    assert!(first["name"].as_str().unwrap().starts_with("workflow_"));
    assert_eq!(first["listed"], false);
    assert!(
        found["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("workflow_")
    );

    let missing = c.call(3, "search_tools", json!({})).await;
    assert_eq!(missing["isError"], true);

    let sets = c.call(4, "list_toolsets", json!({})).await;
    let toolsets = sets["structuredContent"]["toolsets"].as_array().unwrap();
    let all = toolsets.iter().find(|t| t["name"] == "all").expect("all");
    assert_eq!(all["tools"], ToolCatalog::global().tools().len());
    let core = toolsets.iter().find(|t| t["name"] == "core").expect("core");
    assert_eq!(core["enabled"], true);
    c.close().await;
}

#[tokio::test]
async fn older_revisions_get_no_structured_fields() {
    let mut c = client();
    c.initialize("2025-03-26").await;
    let listed = c.request(1, "tools/list", json!({})).await;
    for tool in listed["result"]["tools"].as_array().unwrap() {
        assert!(tool.get("outputSchema").is_none());
        assert!(tool.get("title").is_none());
    }
    let result = c.call(2, "list_toolsets", json!({})).await;
    assert!(result.get("structuredContent").is_none());
    assert!(result["content"][0]["text"].is_string());
    c.close().await;
}

#[tokio::test]
async fn tools_list_advertises_repo_ref_string_forms() {
    let mut c = client();
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let tools = list_all(&mut c, 1).await;
    for name in ["query_log", "ref_list_refs", "write_land"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name}"));
        let repo_ref = &tool["inputSchema"]["$defs"]["bee.v1.RepoRef"];
        assert_eq!(repo_ref["anyOf"][0]["type"], "object");
        assert_eq!(repo_ref["anyOf"][1]["type"], "string");
        assert!(
            repo_ref["description"]
                .as_str()
                .unwrap()
                .contains("org/name")
        );
    }
    c.close().await;
}

#[tokio::test]
async fn configuration_is_validated() {
    let mut config = offline_config();
    config.toolsets = vec!["core".into(), "bogus".into()];
    let error = McpServer::new(config).expect_err("unknown toolset");
    assert!(error.message.contains("bogus"));

    let mut config = offline_config();
    config.page_size = 0;
    assert!(McpServer::new(config).is_err());

    // Output schemas are off by default...
    let mut config = offline_config();
    config.toolsets = vec!["all".into()];
    config.page_size = 100_000;
    let mut c = StdioClient::start(McpServer::new(config).expect("config"));
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let listed = c.request(1, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), ToolCatalog::global().tools().len() + 3);
    assert!(tools.iter().all(|t| t.get("outputSchema").is_none()));
    c.close().await;

    // ...and every RPC tool carries one when asked.
    let mut config = offline_config();
    config.output_schemas = true;
    let mut c = StdioClient::start(McpServer::new(config).expect("config"));
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let listed = c.request(1, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    for tool in &tools[3..] {
        assert_eq!(tool["outputSchema"]["type"], "object", "{}", tool["name"]);
    }
    c.close().await;
}
