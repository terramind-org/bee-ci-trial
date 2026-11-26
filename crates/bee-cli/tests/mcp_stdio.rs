//! `bee mcp`: the stdio MCP server of SPEC §12.4, run as the real binary against a fake
//! Connect server. Tools call the endpoint and token every other command uses.

use crate::support;

use bee_proto::errors::error_detail;
use bee_proto::v1::ErrorReason;
use serde_json::{Value, json};
use support::{Bee, FakeServer, Reply, sample_response, validate_error};

fn request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// Runs `bee mcp <args>` with these JSON-RPC lines on stdin; returns the responses by id.
fn session(server: &FakeServer, args: &[&str], lines: &[String]) -> Vec<Value> {
    let mut argv = vec![
        "mcp",
        "--endpoint",
        server.endpoint.as_str(),
        "--token",
        "t",
    ];
    argv.extend(args);
    let init = request(
        0,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "test", "version": "1"}}),
    );
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();
    let mut input = vec![init, initialized];
    input.extend(lines.iter().cloned());
    let (run, _dir) = Bee::new(&argv).stdin(&(input.join("\n") + "\n")).run();
    assert_eq!(run.code, 0, "{run:?}");
    run.lines()
}

fn response(responses: &[Value], id: u64) -> Value {
    responses
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no response {id} in {responses:#?}"))["result"]
        .clone()
}

#[test]
fn mcp_serves_tools_over_stdio_with_the_cli_endpoint_and_token() {
    let server = FakeServer::start();
    let responses = session(
        &server,
        &[],
        &[
            request(1, "tools/list", json!({})),
            request(
                2,
                "tools/call",
                json!({"name": "query_log", "arguments": {"repo": {"fullName": "dev/playground"}}}),
            ),
        ],
    );
    let init = response(&responses, 0);
    assert_eq!(init["capabilities"]["tools"]["listChanged"], true, "{init}");
    let tools = response(&responses, 1)["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        names.contains(&"auth_who_am_i") && names.contains(&"search_tools"),
        "{names:?}"
    );
    let call = response(&responses, 2);
    assert_eq!(call["isError"], false, "{call}");
    assert_eq!(
        call["structuredContent"],
        sample_response("bee.v1.LogResponse")
    );
    let got = server.last();
    assert_eq!(got.path, "/bee.v1.QueryService/Log");
    assert_eq!(got.authorization.as_deref(), Some("Bearer t"));
    assert_eq!(got.messages[0]["repo"]["fullName"], "dev/playground");
}

#[test]
fn mcp_errors_carry_the_reason_and_toolsets_select_the_list() {
    let server = FakeServer::start();
    server.reply(
        "/bee.v1.IssueService/GetIssue",
        Reply::Error(error_detail(ErrorReason::NotFound, "issue #9 not found")),
    );
    let responses = session(
        &server,
        &["--toolsets", "ci"],
        &[
            request(1, "tools/list", json!({})),
            request(
                2,
                "tools/call",
                json!({"name": "issue_get_issue", "arguments": {"number": "9"}}),
            ),
        ],
    );
    let names: Vec<String> = response(&responses, 1)["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("workflow_")),
        "{names:?}"
    );
    assert!(!names.iter().any(|n| n == "query_log"), "{names:?}");
    // Tools outside the listed toolsets can still be called directly.
    let call = response(&responses, 2);
    assert_eq!(call["isError"], true, "{call}");
    assert_eq!(call["structuredContent"]["reason"], "NOT_FOUND");
    assert_eq!(call["structuredContent"]["retryable"], false);
}

#[test]
fn mcp_rejects_unknown_toolsets_and_unix_endpoints_as_usage_errors() {
    let (run, _dir) = Bee::new(&[
        "mcp",
        "--json",
        "--endpoint",
        "http://127.0.0.1:9",
        "--toolsets",
        "nope",
    ])
    .run();
    assert_eq!(run.code, 2, "{run:?}");
    let error = validate_error(&run.json());
    assert_eq!(error["reason"], "INVALID_ARGUMENT");
    assert_eq!(error["payload"]["subcode"], "unknown_toolset");
    let (run, _dir) = Bee::new(&["mcp", "--json", "--endpoint", "unix:/tmp/bee.sock"]).run();
    assert_eq!(run.code, 2, "{run:?}");
    assert_eq!(
        validate_error(&run.json())["payload"]["subcode"],
        "unsupported_endpoint"
    );
}
