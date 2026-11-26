//! The MCP form: `/mcp` (Streamable HTTP) on the server, and `bee mcp` (stdio) as a child
//! process for a sample.

use std::path::Path;
use std::time::Duration;

use bee_conformance::parity::naming::mcp_tool;
use bee_conformance::parity::normalize::{ErrorFacts, Outcome};
use bee_proto::methods::{MethodDescriptor, MethodKind};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::api::{Client, is_event_stream};

/// Protocol revision the suite speaks (structured tool results).
pub const PROTOCOL: &str = "2025-06-18";

/// An initialised `/mcp` session.
pub struct HttpSession {
    url: String,
    token: String,
    session: String,
    next_id: u64,
}

/// What a tool call returned, with the contract problems seen.
pub struct McpResult {
    /// The outcome.
    pub outcome: Outcome,
    /// Problems with the result's shape.
    pub problems: Vec<String>,
}

async fn post(
    http: &Client,
    url: &str,
    token: &str,
    session: Option<&str>,
    body: &Value,
) -> Result<(u16, Option<String>, Vec<Value>), String> {
    let mut req = http::Request::builder()
        .method("POST")
        .uri(format!("{url}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {token}"));
    if let Some(s) = session {
        req = req
            .header("mcp-session-id", s)
            .header("mcp-protocol-version", PROTOCOL);
    }
    let req = req
        .body(Full::new(Bytes::from(body.to_string())))
        .map_err(|e| e.to_string())?;
    let resp = tokio::time::timeout(Duration::from_secs(180), http.request(req))
        .await
        .map_err(|_| "MCP request timed out".to_owned())?
        .map_err(|e| format!("MCP request failed: {e}"))?;
    let status = resp.status().as_u16();
    let session = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("text/event-stream"));
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    let messages = if bytes.is_empty() {
        Vec::new()
    } else if sse {
        text.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str(d.trim()).ok())
            .collect()
    } else {
        vec![serde_json::from_str(&text).map_err(|e| format!("MCP body ({e}): {text}"))?]
    };
    Ok((status, session, messages))
}

impl HttpSession {
    /// `initialize` + `notifications/initialized`.
    pub async fn open(http: &Client, url: &str, token: &str) -> Result<Self, String> {
        let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": PROTOCOL, "capabilities": {},
            "clientInfo": {"name": "bee-conformance", "version": "1"}}});
        let (status, session, _) = post(http, url, token, None, &init).await?;
        let session = session.ok_or(format!("initialize answered {status} without a session"))?;
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        post(http, url, token, Some(&session), &note).await?;
        Ok(Self {
            url: url.to_owned(),
            token: token.to_owned(),
            session,
            next_id: 1,
        })
    }

    /// One JSON-RPC request; returns its `result` (or the JSON-RPC error as `Err`).
    pub async fn request(
        &mut self,
        http: &Client,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let (status, _, messages) =
            post(http, &self.url, &self.token, Some(&self.session), &body).await?;
        let response = messages
            .into_iter()
            .find(|m| m["id"] == id)
            .ok_or(format!("{method}: HTTP {status} without a response"))?;
        if let Some(e) = response.get("error") {
            return Err(format!("{method}: JSON-RPC error {e}"));
        }
        Ok(response["result"].clone())
    }

    /// Calls the tool of `m`.
    pub async fn call(
        &mut self,
        http: &Client,
        m: &MethodDescriptor,
        request: &Value,
        max_items: Option<u64>,
    ) -> McpResult {
        let params = tool_params(m, request, max_items);
        match self.request(http, "tools/call", params).await {
            Ok(result) => read_result(m, &result),
            Err(e) => McpResult {
                outcome: transport(&e),
                problems: vec![e],
            },
        }
    }
}

/// The `tools/call` params for `m`.
pub fn tool_params(m: &MethodDescriptor, request: &Value, max_items: Option<u64>) -> Value {
    let mut arguments = match (m.kind, request) {
        (MethodKind::ClientStreaming, Value::Array(items)) => json!({ "messages": items }),
        (_, v) => v.clone(),
    };
    if m.kind == MethodKind::ServerStreaming
        && let Some(o) = arguments.as_object_mut()
    {
        if let Some(n) = max_items {
            o.insert("max_items".into(), json!(n));
        }
        if is_event_stream(m) {
            o.insert("timeout_ms".into(), json!(5000));
        }
    }
    json!({ "name": mcp_tool(m.service, m.method), "arguments": arguments })
}

fn transport(message: &str) -> Outcome {
    Outcome::Err(ErrorFacts {
        reason: "TRANSPORT".into(),
        retryable: false,
        subcode: String::new(),
        message: message.to_owned(),
    })
}

/// Reads a `tools/call` result: `structuredContent` is the response (unary), `{"items"}`
/// (streams), or the error document when `isError`.
pub fn read_result(m: &MethodDescriptor, result: &Value) -> McpResult {
    let mut problems = Vec::new();
    let structured = &result["structuredContent"];
    if structured.is_null() {
        problems.push("result without structuredContent".into());
    }
    if result["content"][0]["type"] != "text" {
        problems.push("result without a text content block".into());
    }
    let outcome = if result["isError"] == true {
        Outcome::Err(ErrorFacts {
            reason: structured["reason"].as_str().unwrap_or_default().to_owned(),
            retryable: structured["retryable"].as_bool().unwrap_or(false),
            subcode: structured["payload"]["subcode"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            message: structured["message"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
    } else if m.kind == MethodKind::ServerStreaming {
        let items = structured["items"].as_array().cloned().unwrap_or_else(|| {
            problems.push("stream result without items".into());
            Vec::new()
        });
        if structured["count"].as_u64() != Some(items.len() as u64) {
            problems.push(format!(
                "count {} for {} items",
                structured["count"],
                items.len()
            ));
        }
        Outcome::Ok(items)
    } else {
        Outcome::Ok(vec![structured.clone()])
    };
    McpResult { outcome, problems }
}

/// Lists every tool of a session after enabling the `all` toolset.
pub async fn list_all_tools(http: &Client, url: &str, token: &str) -> Result<Vec<Value>, String> {
    let mut s = HttpSession::open(http, url, token).await?;
    s.request(
        http,
        "tools/call",
        json!({"name": "enable_toolset", "arguments": {"toolsets": ["all"]}}),
    )
    .await?;
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = match &cursor {
            Some(c) => json!({ "cursor": c }),
            None => json!({}),
        };
        let page = s.request(http, "tools/list", params).await?;
        tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
        match page["nextCursor"].as_str() {
            Some(c) => cursor = Some(c.to_owned()),
            None => break,
        }
    }
    Ok(tools)
}

/// `bee mcp` over stdio.
pub struct StdioSession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
}

impl StdioSession {
    /// Starts `bee mcp` against `url` and initialises it.
    pub async fn open(url: &str, token: &str, config_dir: &Path) -> Result<Self, String> {
        let (program, as_cli) = crate::cli::program();
        let mut cmd = tokio::process::Command::new(program);
        cmd.arg("mcp")
            .env("BEE_ENDPOINT", url)
            .env("BEE_TOKEN", token)
            .env("BEE_MCP_TOOLSETS", "all")
            .env("BEE_CONFIG_DIR", config_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if as_cli {
            cmd.env(crate::cli::AS_CLI, "1");
        }
        let mut child = cmd.spawn().map_err(|e| format!("bee mcp: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut s = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 0,
        };
        s.request(
            "initialize",
            json!({"protocolVersion": PROTOCOL, "capabilities": {},
                   "clientInfo": {"name": "bee-conformance", "version": "1"}}),
        )
        .await?;
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        Ok(s)
    }

    async fn send(&mut self, v: &Value) -> Result<(), String> {
        let mut line = v.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    /// One request; returns its result.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(180), self.lines.next_line())
                .await
                .map_err(|_| format!("bee mcp: no answer to {method}"))?
                .map_err(|e| e.to_string())?
                .ok_or("bee mcp exited")?;
            let v: Value = serde_json::from_str(&line).map_err(|e| format!("{e}: {line}"))?;
            if v["id"] == id {
                if let Some(e) = v.get("error") {
                    return Err(format!("{method}: JSON-RPC error {e}"));
                }
                return Ok(v["result"].clone());
            }
        }
    }

    /// Calls the tool of `m`.
    pub async fn call(
        &mut self,
        m: &MethodDescriptor,
        request: &Value,
        max_items: Option<u64>,
    ) -> McpResult {
        match self
            .request("tools/call", tool_params(m, request, max_items))
            .await
        {
            Ok(result) => read_result(m, &result),
            Err(e) => McpResult {
                outcome: transport(&e),
                problems: vec![e],
            },
        }
    }

    /// Ends the process.
    pub async fn close(mut self) {
        let _ = self.stdin.shutdown().await;
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}
