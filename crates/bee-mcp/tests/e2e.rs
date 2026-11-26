//! End to end: MCP clients drive the server over stdio pipes and Streamable HTTP; the server
//! calls a fake bee server that speaks the Connect HTTP/JSON shape.

use crate::common;
use std::time::Duration;

use bee_mcp::catalog::ToolCatalog;
use bee_mcp::protocol::LATEST_PROTOCOL_VERSION;
use bee_mcp::{ClientConfig, McpConfig, McpServer, call_rpc_json, error_document};
use bee_proto::envelope;
use common::{HttpMcp, StdioClient, TOKEN, fake_server, merge_conflict_detail, validate};
use http::{Method, StatusCode};
use serde_json::{Value, json};

fn check_output(tool: &str, structured: &Value) {
    let schema = ToolCatalog::global()
        .get(tool)
        .expect("tool")
        .output_schema();
    validate(schema, schema, structured, "#")
        .unwrap_or_else(|e| panic!("{tool} result does not match its outputSchema: {e}"));
}

fn check_error_document(doc: &Value) {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1/error.schema.json");
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(root).unwrap()).unwrap();
    let envelope = json!({"schema": "bee.v1.error", "error": doc});
    validate(&schema, &schema, &envelope, "#")
        .unwrap_or_else(|e| panic!("not a CLI error document: {e}"));
}

#[tokio::test]
async fn stdio_unary_call_round_trips() {
    let fake = fake_server().await;
    let mut c = StdioClient::start(fake.mcp_server());
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let result = c
        .call(
            1,
            "query_log",
            json!({"repo": {"fullName": "acme/web"}, "firstParent": true}),
        )
        .await;
    assert_eq!(result["isError"], false, "{result}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["page"]["nextPageToken"], "after:acme/web");
    assert_eq!(structured["changesets"][0]["generation"], "7");
    check_output("query_log", structured);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Query.Log: ok"));
    assert!(text.contains("after:acme/web"));

    let seen = fake.last("/bee.v1.QueryService/Log");
    assert_eq!(seen.content_type, "application/json");
    assert_eq!(
        seen.authorization.as_deref(),
        Some(&*format!("Bearer {TOKEN}"))
    );
    assert_eq!(seen.protocol_version.as_deref(), Some("1"));
    let sent: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(
        sent,
        json!({"repo": {"fullName": "acme/web"}, "firstParent": true})
    );
    c.close().await;
}

#[tokio::test]
async fn repo_ref_strings_are_normalized_in_every_mcp_argument_position() {
    let fake = fake_server().await;
    let mut c = StdioClient::start(fake.mcp_server());
    c.initialize(LATEST_PROTOCOL_VERSION).await;

    // The compact full-name and ULID forms reach the HTTP/JSON service as their object forms.
    let full_name = c.call(1, "query_log", json!({"repo": "acme/web"})).await;
    assert_eq!(full_name["isError"], false, "{full_name}");
    assert_eq!(
        serde_json::from_slice::<Value>(&fake.last("/bee.v1.QueryService/Log").body).unwrap(),
        json!({"repo": {"fullName": "acme/web"}})
    );
    let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let by_id = c.call(2, "query_log", json!({"repo": id})).await;
    assert_eq!(by_id["isError"], false, "{by_id}");
    assert_eq!(
        serde_json::from_slice::<Value>(&fake.last("/bee.v1.QueryService/Log").body).unwrap(),
        json!({"repo": {"id": id}})
    );

    // Existing proto3-JSON objects are unchanged.
    for (call, repo) in [(3, json!({"fullName": "acme/web"})), (4, json!({"id": id}))] {
        let result = c
            .call(call, "query_log", json!({"repo": repo.clone()}))
            .await;
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(
            serde_json::from_slice::<Value>(&fake.last("/bee.v1.QueryService/Log").body).unwrap()["repo"],
            repo
        );
    }

    // `blockedBy.repo` is a nested RepoRef, distinct from the request's own repo.
    let nested = c
        .call(
            5,
            "issue_add_issue_dependency",
            json!({
                "repo": "acme/web",
                "number": "1",
                "blockedBy": {"repo": "acme/lib", "number": "2"},
            }),
        )
        .await;
    assert_eq!(nested["structuredContent"]["reason"], "UNIMPLEMENTED");
    let sent: Value =
        serde_json::from_slice(&fake.last("/bee.v1.IssueService/AddIssueDependency").body).unwrap();
    assert_eq!(sent["repo"], json!({"fullName": "acme/web"}));
    assert_eq!(sent["blockedBy"]["repo"], json!({"fullName": "acme/lib"}));

    let before = fake.seen().len();
    let bad = c.call(6, "query_log", json!({"repo": "not a repo"})).await;
    assert_eq!(bad["structuredContent"]["reason"], "INVALID_ARGUMENT");
    assert_eq!(
        bad["structuredContent"]["payload"]["subcode"],
        "bad_repo_ref"
    );
    let message = bad["structuredContent"]["message"].as_str().unwrap();
    assert!(message.contains("repo") && message.contains("org/name") && message.contains("ULID"));
    assert_eq!(
        fake.seen().len(),
        before,
        "bad strings must not reach the server"
    );
    c.close().await;
}

#[tokio::test]
async fn stdio_land_conflict_is_a_tool_error_with_the_error_detail() {
    let fake = fake_server().await;
    let mut c = StdioClient::start(fake.mcp_server());
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let result = c
        .call(
            1,
            "write_land",
            json!({"repo": {"fullName": "acme/web"}, "ref": "main", "head": {"raw": "AAAA"}}),
        )
        .await;
    assert_eq!(result["isError"], true);
    let doc = &result["structuredContent"];
    assert_eq!(doc["reason"], "MERGE_CONFLICT");
    assert_eq!(doc["retryable"], false);
    assert_eq!(doc["message"], "main moved: 1 conflict");
    assert_eq!(doc["payload"]["requestId"], "req-42");
    assert_eq!(
        doc["payload"]["conflicts"]["conflicts"][0]["kind"],
        "CONFLICT_KIND_CONTENT"
    );
    assert_eq!(doc, &error_document(&merge_conflict_detail()));
    check_error_document(doc);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("MERGE_CONFLICT (not retryable)"));
    c.close().await;
}

#[tokio::test]
async fn bad_arguments_and_transport_failures_carry_reasons() {
    let fake = fake_server().await;
    let mut c = StdioClient::start(fake.mcp_server());
    c.initialize(LATEST_PROTOCOL_VERSION).await;

    // Not a LogRequest: rejected locally, never sent.
    let before = fake.seen().len();
    let bad = c.call(1, "query_log", json!({"firstParent": "yes"})).await;
    assert_eq!(bad["isError"], true);
    assert_eq!(bad["structuredContent"]["reason"], "INVALID_ARGUMENT");
    assert_eq!(bad["structuredContent"]["payload"]["subcode"], "bad_json");
    assert_eq!(fake.seen().len(), before);

    // A non-Connect 503 maps to UNAVAILABLE, which is retryable.
    let down = c
        .call(
            2,
            "ref_list_refs",
            json!({"repo": {"fullName": "acme/web"}}),
        )
        .await;
    assert_eq!(down["structuredContent"]["reason"], "UNAVAILABLE");
    assert_eq!(down["structuredContent"]["retryable"], true);
    check_error_document(&down["structuredContent"]);

    // A Connect error without its own detail keeps the server's reason mapping.
    let missing = c.call(3, "issue_create_issue", json!({})).await;
    assert_eq!(missing["structuredContent"]["reason"], "UNIMPLEMENTED");
    c.close().await;

    // Nobody listening: UNAVAILABLE.
    let mut config = McpConfig::new(ClientConfig::new("http://127.0.0.1:1"));
    config.client.request_timeout = Duration::from_secs(5);
    let mut c = StdioClient::start(McpServer::new(config).unwrap());
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let result = c.call(1, "auth_who_am_i", json!({})).await;
    assert_eq!(result["structuredContent"]["reason"], "UNAVAILABLE");
    assert_eq!(result["structuredContent"]["retryable"], true);
    c.close().await;
}

#[tokio::test]
async fn server_streams_are_collected_with_limits() {
    let fake = fake_server().await;
    let mut c = StdioClient::start(fake.mcp_server());
    c.initialize(LATEST_PROTOCOL_VERSION).await;

    let all = c
        .call(1, "query_diff", json!({"repo": {"fullName": "acme/web"}}))
        .await;
    assert_eq!(all["isError"], false, "{all}");
    let s = &all["structuredContent"];
    assert_eq!(s["count"], 3);
    assert_eq!(s["truncated"], false);
    assert_eq!(s["items"][2]["additions"], 3);
    check_output("query_diff", s);
    let seen = fake.last("/bee.v1.QueryService/Diff");
    assert_eq!(seen.content_type, "application/connect+json");
    let frames = envelope::decode_all(&seen.body, 1 << 20).unwrap();
    assert_eq!(frames.len(), 1);
    let sent: Value = serde_json::from_slice(&frames[0].data).unwrap();
    assert_eq!(sent, json!({"repo": {"fullName": "acme/web"}}));

    let two = c
        .call(
            2,
            "query_diff",
            json!({"repo": {"fullName": "acme/web"}, "max_items": 2}),
        )
        .await;
    let s = &two["structuredContent"];
    assert_eq!(s["count"], 2);
    assert_eq!(s["truncated"], true);
    assert_eq!(s["truncatedReason"], "max_items");
    assert!(
        two["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("truncated at max_items")
    );
    let sent: Value = serde_json::from_slice(
        &envelope::decode_all(&fake.last("/bee.v1.QueryService/Diff").body, 1 << 20).unwrap()[0]
            .data,
    )
    .unwrap();
    assert!(
        sent.get("max_items").is_none(),
        "limits must not reach the server"
    );

    let open = c
        .call(
            3,
            "event_subscribe",
            json!({"repo": {"fullName": "acme/web"}, "timeout_ms": 300}),
        )
        .await;
    let s = &open["structuredContent"];
    assert_eq!(s["count"], 1);
    assert_eq!(s["truncatedReason"], "timeout");
    assert_eq!(s["items"][0]["kind"], "ref.updated");
    check_output("event_subscribe", s);

    let bad = c.call(4, "query_diff", json!({"max_items": 0})).await;
    assert_eq!(bad["structuredContent"]["reason"], "INVALID_ARGUMENT");

    let failed = c.call(5, "query_read_path", json!({})).await;
    assert_eq!(failed["isError"], true);
    assert_eq!(failed["structuredContent"]["reason"], "UNAVAILABLE");
    assert!(
        failed["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("after 1 stream message")
    );

    let upload = c
        .call(6, "write_upload_objects", json!({"messages": [{}, {}, {}]}))
        .await;
    assert_eq!(upload["isError"], false, "{upload}");
    assert_eq!(upload["structuredContent"]["stored"], 3);
    let no_messages = c.call(7, "write_upload_objects", json!({})).await;
    assert_eq!(
        no_messages["structuredContent"]["reason"],
        "INVALID_ARGUMENT"
    );
    c.close().await;
}

#[tokio::test]
async fn stdio_cancellation_stops_a_running_call() {
    let fake = fake_server().await;
    let mut c = StdioClient::start(fake.mcp_server());
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    c.send(
        &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {
            "name": "event_subscribe", "arguments": {"timeout_ms": 60000}
        }}),
    )
    .await;
    // Calls run concurrently: ping answers while the subscription is open.
    let pong = c.request(10, "ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
    c.notify(
        "notifications/cancelled",
        json!({"requestId": 9, "reason": "user"}),
    )
    .await;
    let pong = c.request(11, "ping", json!({})).await;
    assert_eq!(pong["id"], 11);
    // The cancelled call never answers, and closing does not wait for it.
    c.close().await;
}

#[tokio::test]
async fn call_rpc_json_is_the_shared_code_path() {
    let fake = fake_server().await;
    let mut config = ClientConfig::new(&fake.endpoint);
    config.token = Some(TOKEN.into());
    let request = json!({"repo": {"fullName": "acme/api"}});
    let direct = call_rpc_json(&config, "QueryService", "Log", &request)
        .await
        .expect("log");

    let mut c = StdioClient::start(fake.mcp_server());
    c.initialize(LATEST_PROTOCOL_VERSION).await;
    let via_mcp = c.call(1, "query_log", request.clone()).await;
    assert_eq!(via_mcp["structuredContent"], direct);
    c.close().await;

    let error = call_rpc_json(&config, "write", "land", &json!({}))
        .await
        .expect_err("conflict");
    assert_eq!(error, merge_conflict_detail());

    let unknown = call_rpc_json(&config, "write", "nope", &json!({}))
        .await
        .expect_err("unknown");
    assert_eq!(unknown.reason, "UNIMPLEMENTED");

    let streamed = call_rpc_json(&config, "bee.v1.QueryService", "Diff", &json!({}))
        .await
        .expect("diff");
    assert_eq!(streamed["count"], 3);

    config.token = Some("wrong".into());
    let denied = call_rpc_json(&config, "auth", "WhoAmI", &json!({}))
        .await
        .expect_err("denied");
    assert_eq!(denied.reason, "UNAUTHENTICATED");
}

#[tokio::test]
async fn http_transport_round_trips_at_mcp() {
    let fake = fake_server().await;
    let mcp = HttpMcp::start(fake.mcp_config()).await;
    let session = mcp.initialize(&[]).await;

    let listed = mcp
        .post(
            Some(&session),
            &[],
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.header("content-type"), Some("application/json"));
    let page = listed.json();
    assert!(page["result"]["tools"].as_array().unwrap().len() > 3);
    assert!(page["result"]["nextCursor"].is_string());

    let log = mcp
        .post(
            Some(&session),
            &[],
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": "query_log", "arguments": {"repo": {"fullName": "acme/web"}}
            }}),
        )
        .await
        .json();
    assert_eq!(log["result"]["isError"], false);
    assert_eq!(
        log["result"]["structuredContent"]["page"]["nextPageToken"],
        "after:acme/web"
    );

    let land = mcp
        .post(
            Some(&session),
            &[],
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
                "name": "write_land", "arguments": {}
            }}),
        )
        .await
        .json();
    assert_eq!(land["result"]["isError"], true);
    assert_eq!(
        land["result"]["structuredContent"]["reason"],
        "MERGE_CONFLICT"
    );

    let batch = mcp
        .post(
            Some(&session),
            &[],
            &json!([
                {"jsonrpc": "2.0", "id": 5, "method": "ping"},
                {"jsonrpc": "2.0", "id": 6, "method": "ping"}
            ]),
        )
        .await
        .json();
    assert_eq!(batch.as_array().unwrap().len(), 2);

    let deleted = mcp
        .send(Method::DELETE, &[("mcp-session-id", &session)], None)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = mcp
        .post(
            Some(&session),
            &[],
            &json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}),
        )
        .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_enable_toolset_streams_list_changed_before_the_response() {
    let fake = fake_server().await;
    let mcp = HttpMcp::start(fake.mcp_config()).await;
    let session = mcp.initialize(&[]).await;
    let reply = mcp
        .post(
            Some(&session),
            &[],
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "enable_toolset", "arguments": {"toolsets": ["security"]}
            }}),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("text/event-stream"));
    let messages = reply.sse_messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["method"], "notifications/tools/list_changed");
    assert_eq!(messages[1]["id"], 2);
    assert!(
        messages[1]["result"]["structuredContent"]["added"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[tokio::test]
async fn http_forwards_each_callers_credential_and_binds_sessions_to_it() {
    let fake = fake_server().await;
    let mcp = HttpMcp::start(fake.mcp_config()).await;
    let auth = [("authorization", "Bearer other-token")];
    let session = mcp.initialize(&auth).await;
    let who = mcp
        .post(
            Some(&session),
            &auth,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "auth_who_am_i", "arguments": {}
            }}),
        )
        .await
        .json();
    assert_eq!(
        who["result"]["structuredContent"]["tokenId"],
        "Bearer other-token"
    );

    // The same session id with another credential is not that session.
    let hijack = mcp
        .post(
            Some(&session),
            &[],
            &json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
        )
        .await;
    assert_eq!(hijack.status, StatusCode::NOT_FOUND);

    // Without a header the configured token is used.
    let own = mcp.initialize(&[]).await;
    let who = mcp
        .post(
            Some(&own),
            &[],
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
                "name": "auth_who_am_i", "arguments": {}
            }}),
        )
        .await
        .json();
    assert_eq!(
        who["result"]["structuredContent"]["tokenId"],
        format!("Bearer {TOKEN}")
    );
}

#[tokio::test]
async fn http_transport_rules() {
    let fake = fake_server().await;
    let mut config = fake.mcp_config();
    config.allowed_origins = vec!["https://hub.example.com".into()];
    let mcp = HttpMcp::start(config).await;
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});

    let no_session = mcp.post(None, &[], &ping).await;
    assert_eq!(no_session.status, StatusCode::BAD_REQUEST);
    let unknown = mcp.post(Some("feedface"), &[], &ping).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    let get = mcp
        .send(Method::GET, &[("accept", "text/event-stream")], None)
        .await;
    assert_eq!(get.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(get.header("allow"), Some("POST, DELETE"));

    let evil = mcp
        .post(None, &[("origin", "https://evil.example")], &ping)
        .await;
    assert_eq!(evil.status, StatusCode::FORBIDDEN);
    let allowed = mcp
        .initialize(&[("origin", "https://hub.example.com")])
        .await;
    assert!(!allowed.is_empty());
    let local = mcp.initialize(&[("origin", "http://localhost:3000")]).await;
    assert!(!local.is_empty());

    let session = mcp.initialize(&[]).await;
    let bad_version = mcp
        .send(
            Method::POST,
            &[
                ("content-type", "application/json"),
                ("mcp-session-id", &session),
                ("mcp-protocol-version", "1999-01-01"),
            ],
            Some(&ping),
        )
        .await;
    assert_eq!(bad_version.status, StatusCode::BAD_REQUEST);

    let wrong_type = mcp
        .send(
            Method::POST,
            &[("content-type", "text/plain"), ("mcp-session-id", &session)],
            Some(&ping),
        )
        .await;
    assert_eq!(wrong_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let not_acceptable = mcp
        .send(
            Method::POST,
            &[
                ("content-type", "application/json"),
                ("accept", "text/html"),
                ("mcp-session-id", &session),
            ],
            Some(&ping),
        )
        .await;
    assert_eq!(not_acceptable.status, StatusCode::NOT_ACCEPTABLE);

    let garbage = mcp
        .send(
            Method::POST,
            &[
                ("content-type", "application/json"),
                ("mcp-session-id", &session),
            ],
            None,
        )
        .await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
    assert_eq!(garbage.json()["error"]["code"], -32700);

    let invalid_notification = mcp
        .post(
            Some(&session),
            &[],
            &json!({"jsonrpc": "1.0", "method": "x"}),
        )
        .await;
    assert_eq!(invalid_notification.status, StatusCode::BAD_REQUEST);

    let batched_init = mcp
        .post(
            None,
            &[],
            &json!([{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25"}}]),
        )
        .await;
    assert_eq!(batched_init.status, StatusCode::BAD_REQUEST);

    let ok = mcp.post(Some(&session), &[], &ping).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.json()["result"], json!({}));
}

#[tokio::test]
async fn http_sessions_are_capped() {
    let fake = fake_server().await;
    let mut config = fake.mcp_config();
    config.max_sessions = 2;
    let mcp = HttpMcp::start(config).await;
    let first = mcp.initialize(&[]).await;
    let _second = mcp.initialize(&[]).await;
    let _third = mcp.initialize(&[]).await;
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    assert_eq!(
        mcp.post(Some(&first), &[], &ping).await.status,
        StatusCode::NOT_FOUND
    );
}
