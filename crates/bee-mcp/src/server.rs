//! The transport-independent MCP server: lifecycle, tool listing and tool calls.
//!
//! # Lifecycle
//!
//! A session begins with `initialize`; the server answers with the client's protocol
//! version when it implements it (see [`crate::protocol::SUPPORTED_PROTOCOL_VERSIONS`]) and
//! its newest otherwise. Until then only `initialize` and `ping` are served. Capabilities:
//! `tools` with `listChanged`.
//!
//! # Tools
//!
//! `tools/list` returns the meta-tools first, then every RPC tool in the session's enabled
//! toolsets, in name order, [`McpConfig::page_size`] per page with an opaque `nextCursor`.
//! `tools/call` calls any RPC tool in the catalog, listed or not, so a tool found with
//! `search_tools` can be called directly.
//!
//! Meta-tools:
//! - `search_tools` `{query, limit?}`: matching tools across the whole catalog, with their
//!   toolsets and whether they are listed.
//! - `list_toolsets` `{}`: every toolset with its tool count and whether it is enabled.
//! - `enable_toolset` `{toolsets?: [..], toolset?: "..", tools?: [..]}`: adds toolsets or single
//!   tools to this session's list and sends `notifications/tools/list_changed` when the list
//!   grew.
//!
//! # Results and errors
//!
//! A successful call returns the response JSON as `structuredContent` and as text after a one
//! line summary. A failed RPC is a tool result with `isError: true` whose `structuredContent`
//! is the error document `{reason, message, retryable, payload?}` (SPEC §12.2, see
//! [`crate::client::error_document`]); arguments that do not match the request message fail the
//! same way with `INVALID_ARGUMENT` / `bad_json`, so the model can correct them. Protocol
//! problems (unknown method, unknown tool, malformed params) are JSON-RPC errors.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bee_proto::v1::{ErrorDetail, ErrorReason};
use serde_json::{Map, Value, json};
use tokio::task::AbortHandle;

use crate::catalog::{
    ALL_TOOLSET, DEFAULT_MAX_ITEMS, DEFAULT_TIMEOUT_MS, DEFAULT_TOOLSET, ToolCatalog, ToolDef,
    normalize_repo_ref_arguments,
};
use crate::client::{
    ClientConfig, RpcClient, RpcOutput, StreamLimits, Truncation, error_document,
    error_with_subcode,
};
use crate::protocol::{
    INVALID_PARAMS, INVALID_REQUEST, Incoming, LATEST_PROTOCOL_VERSION, METHOD_NOT_FOUND,
    error_response, has_structured_output, is_supported_version, notification, result_response,
};

/// Name of the meta-tool that searches the catalog.
pub const SEARCH_TOOLS: &str = "search_tools";
/// Name of the meta-tool that lists toolsets.
pub const LIST_TOOLSETS: &str = "list_toolsets";
/// Name of the meta-tool that enables toolsets or tools.
pub const ENABLE_TOOLSET: &str = "enable_toolset";

/// Text renderings longer than this are cut (the full result stays in `structuredContent`).
const MAX_TEXT_BYTES: usize = 256 * 1024;

/// Configuration of an MCP server.
#[derive(Clone, Debug)]
pub struct McpConfig {
    /// How to reach the bee server whose RPCs the tools call.
    pub client: ClientConfig,
    /// Toolsets each session starts with. Default `["core"]`; `["all"]` lists every tool.
    pub toolsets: Vec<String>,
    /// Tools per `tools/list` page. Default 100.
    pub page_size: usize,
    /// Include `outputSchema` in tool definitions (protocol 2025-06-18 and later). Default
    /// false: response schemas pull in every reachable message and roughly quadruple the
    /// size of `tools/list` (the `core` set grows from about 0.3 MB to 1.1 MB), which costs
    /// agents context for little gain. Results always conform to the response message's
    /// schema in `schemas/v1/messages/`, which each description names. `BEE_MCP_OUTPUT_SCHEMAS=1`
    /// turns this on in [`McpConfig::from_env`].
    pub output_schemas: bool,
    /// Upper bound on a streaming tool's `max_items`. Default 10,000.
    pub max_stream_items: u64,
    /// Upper bound on a streaming tool's `timeout_ms`. Default 300,000 (5 minutes).
    pub max_stream_timeout_ms: u64,
    /// Streamable HTTP: extra `Origin` values to accept, exactly as browsers send them
    /// (`https://hub.example.com`). Loopback origins and the request's own host are always
    /// accepted; `*` accepts any origin.
    pub allowed_origins: Vec<String>,
    /// Streamable HTTP: most sessions kept at once; the least recently used is dropped
    /// beyond it. Default 1,024.
    pub max_sessions: usize,
    /// Streamable HTTP: sessions idle this long are dropped. Default one hour.
    pub session_idle_timeout: Duration,
    /// Streamable HTTP: largest accepted request body in bytes. Default 16 MiB.
    pub max_body_bytes: usize,
}

impl McpConfig {
    /// Defaults around a client config.
    pub fn new(client: ClientConfig) -> Self {
        Self {
            client,
            toolsets: vec![DEFAULT_TOOLSET.to_owned()],
            page_size: 100,
            output_schemas: false,
            max_stream_items: 10_000,
            max_stream_timeout_ms: 300_000,
            allowed_origins: Vec::new(),
            max_sessions: 1024,
            session_idle_timeout: Duration::from_secs(3600),
            max_body_bytes: 16 << 20,
        }
    }

    /// Reads [`ClientConfig::from_env`] plus `BEE_MCP_TOOLSETS` (comma-separated toolset
    /// names, e.g. `core,ci,security`) and `BEE_MCP_OUTPUT_SCHEMAS` (`1` or `true`).
    pub fn from_env() -> Self {
        let mut config = Self::new(ClientConfig::from_env());
        config.output_schemas = std::env::var("BEE_MCP_OUTPUT_SCHEMAS")
            .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
        if let Ok(list) = std::env::var("BEE_MCP_TOOLSETS") {
            let sets: Vec<String> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if !sets.is_empty() {
                config.toolsets = sets;
            }
        }
        config
    }
}

/// An invalid [`McpConfig`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Per-call context supplied by the transport.
#[derive(Clone, Debug, Default)]
pub struct CallContext {
    /// `Authorization` header value to forward instead of the configured token (Streamable
    /// HTTP forwards each caller's own credential).
    pub authorization: Option<String>,
}

/// What handling one message produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Handled {
    /// The response, for requests and invalid messages.
    pub response: Option<Value>,
    /// Notifications to deliver before the response.
    pub notifications: Vec<Value>,
}

#[derive(Debug)]
struct SessionState {
    protocol_version: Option<String>,
    initialized_notified: bool,
    toolsets: BTreeSet<String>,
    tools: BTreeSet<String>,
}

/// One client's session: negotiated version and enabled tools.
#[derive(Debug)]
pub struct Session {
    state: Mutex<SessionState>,
    inflight: Mutex<HashMap<String, AbortHandle>>,
}

impl Session {
    fn new(toolsets: &[String]) -> Self {
        Self {
            state: Mutex::new(SessionState {
                protocol_version: None,
                initialized_notified: false,
                toolsets: toolsets.iter().cloned().collect(),
                tools: BTreeSet::new(),
            }),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    fn state(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The negotiated protocol version, once `initialize` succeeded.
    pub fn protocol_version(&self) -> Option<String> {
        self.state().protocol_version.clone()
    }

    /// True once the client sent `notifications/initialized`.
    pub fn initialized_notified(&self) -> bool {
        self.state().initialized_notified
    }

    /// Toolsets enabled in this session.
    pub fn enabled_toolsets(&self) -> Vec<String> {
        self.state().toolsets.iter().cloned().collect()
    }

    /// Registers a cancellable in-flight request.
    pub fn track(&self, id: &Value, handle: AbortHandle) {
        self.inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string(), handle);
    }

    /// Forgets a finished request.
    pub fn untrack(&self, id: &Value) {
        self.inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id.to_string());
    }

    /// Aborts an in-flight request (`notifications/cancelled`). True if it was running.
    pub fn cancel(&self, id: &Value) -> bool {
        let handle = self
            .inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id.to_string());
        handle.map(|h| h.abort()).is_some()
    }
}

struct Inner {
    config: McpConfig,
    client: RpcClient,
}

/// An MCP server over the bee HTTP/JSON surface. Cheap to clone.
#[derive(Clone)]
pub struct McpServer {
    inner: Arc<Inner>,
}

impl fmt::Debug for McpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServer")
            .field("endpoint", &self.inner.client.endpoint())
            .field("toolsets", &self.inner.config.toolsets)
            .finish()
    }
}

impl McpServer {
    /// A server for `config`. Fails if a configured toolset does not exist.
    pub fn new(config: McpConfig) -> Result<Self, ConfigError> {
        let unknown = crate::catalog::unknown_toolsets(config.toolsets.iter().map(String::as_str));
        if !unknown.is_empty() {
            return Err(ConfigError {
                message: format!(
                    "unknown toolset(s) {}; known: {}",
                    unknown.into_iter().collect::<Vec<_>>().join(", "),
                    ToolCatalog::global().toolset_names().join(", ")
                ),
            });
        }
        if config.page_size == 0 {
            return Err(ConfigError {
                message: "page_size must be at least 1".into(),
            });
        }
        let client = RpcClient::new(&config.client);
        Ok(Self {
            inner: Arc::new(Inner { config, client }),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &McpConfig {
        &self.inner.config
    }

    /// A fresh session with the configured toolsets.
    pub fn new_session(&self) -> Session {
        Session::new(&self.inner.config.toolsets)
    }

    /// Handles one classified message.
    pub async fn handle(
        &self,
        session: &Session,
        message: Incoming,
        context: &CallContext,
    ) -> Handled {
        match message {
            Incoming::Invalid(response) => Handled {
                response: Some(response),
                notifications: Vec::new(),
            },
            Incoming::Response => Handled::default(),
            Incoming::Notification { method, params } => {
                self.notification(session, &method, &params);
                Handled::default()
            }
            Incoming::Request { id, method, params } => {
                self.request(session, id, &method, params, context).await
            }
        }
    }

    fn notification(&self, session: &Session, method: &str, params: &Value) {
        match method {
            "notifications/initialized" => session.state().initialized_notified = true,
            "notifications/cancelled" => {
                if let Some(id) = params.get("requestId") {
                    session.cancel(id);
                }
            }
            _ => {}
        }
    }

    async fn request(
        &self,
        session: &Session,
        id: Value,
        method: &str,
        params: Value,
        context: &CallContext,
    ) -> Handled {
        let mut handled = Handled::default();
        let initialized = session.protocol_version().is_some();
        let outcome: Result<Value, (i64, String)> = match method {
            "initialize" => self.initialize(session, &params),
            "ping" => Ok(json!({})),
            _ if !initialized => Err((
                INVALID_REQUEST,
                format!("{method} before initialize: send initialize first"),
            )),
            "tools/list" => self.list_tools(session, &params),
            "tools/call" => {
                self.call_tool(session, &params, context, &mut handled.notifications)
                    .await
            }
            _ => Err((METHOD_NOT_FOUND, format!("method {method} is not served"))),
        };
        handled.response = Some(match outcome {
            Ok(result) => result_response(id, result),
            Err((code, message)) => error_response(id, code, &message, None),
        });
        handled
    }

    fn initialize(&self, session: &Session, params: &Value) -> Result<Value, (i64, String)> {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or((
                INVALID_PARAMS,
                "initialize needs a string protocolVersion".to_owned(),
            ))?;
        let version = if is_supported_version(requested) {
            requested
        } else {
            LATEST_PROTOCOL_VERSION
        };
        {
            let mut state = session.state();
            if state.protocol_version.is_some() {
                return Err((
                    INVALID_REQUEST,
                    "this session is already initialized".to_owned(),
                ));
            }
            state.protocol_version = Some(version.to_owned());
        }
        let mut server_info = json!({
            "name": "bee-mcp",
            "version": env!("CARGO_PKG_VERSION"),
        });
        if has_structured_output(version)
            && let Some(object) = server_info.as_object_mut()
        {
            object.insert("title".into(), json!("bee"));
        }
        Ok(json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": server_info,
            "instructions": instructions(&session.enabled_toolsets()),
        }))
    }

    fn list_tools(&self, session: &Session, params: &Value) -> Result<Value, (i64, String)> {
        let offset = match params.get("cursor") {
            None | Some(Value::Null) => 0,
            Some(Value::String(cursor)) => parse_cursor(cursor)
                .ok_or((INVALID_PARAMS, format!("invalid cursor {cursor:?}")))?,
            Some(_) => return Err((INVALID_PARAMS, "cursor must be a string".to_owned())),
        };
        let version = session
            .protocol_version()
            .unwrap_or_else(|| LATEST_PROTOCOL_VERSION.to_owned());
        let structured = has_structured_output(&version);
        let names = self.listed_names(session);
        if offset > names.len() {
            return Err((INVALID_PARAMS, "cursor is past the end of the list".into()));
        }
        let page = self.inner.config.page_size;
        let end = (offset + page).min(names.len());
        let catalog = ToolCatalog::global();
        let tools: Vec<Value> = names[offset..end]
            .iter()
            .filter_map(|name| match name.as_str() {
                SEARCH_TOOLS | LIST_TOOLSETS | ENABLE_TOOLSET => meta_tool(name, structured),
                _ => catalog
                    .get(name)
                    .map(|t| t.to_json(structured, self.inner.config.output_schemas)),
            })
            .collect();
        let mut result = json!({"tools": tools});
        if end < names.len()
            && let Some(object) = result.as_object_mut()
        {
            object.insert("nextCursor".into(), json!(format!("page:{end}")));
        }
        Ok(result)
    }

    /// Names in the session's list: meta-tools, then enabled RPC tools by name.
    fn listed_names(&self, session: &Session) -> Vec<String> {
        let state = session.state();
        let catalog = ToolCatalog::global();
        let mut names: Vec<String> = [SEARCH_TOOLS, LIST_TOOLSETS, ENABLE_TOOLSET]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        names.extend(
            catalog
                .tools()
                .iter()
                .filter(|t| is_enabled(&state, t))
                .map(|t| t.name.clone()),
        );
        names
    }

    async fn call_tool(
        &self,
        session: &Session,
        params: &Value,
        context: &CallContext,
        notifications: &mut Vec<Value>,
    ) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((INVALID_PARAMS, "tools/call needs a string name".to_owned()))?;
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(args @ Value::Object(_)) => args.clone(),
            Some(_) => return Err((INVALID_PARAMS, "arguments must be an object".to_owned())),
        };
        let structured = session
            .protocol_version()
            .is_some_and(|v| has_structured_output(&v));
        let result = match name {
            SEARCH_TOOLS => self.search_tools(session, &arguments),
            LIST_TOOLSETS => self.list_toolsets(session),
            ENABLE_TOOLSET => self.enable_toolset(session, &arguments, notifications),
            _ => {
                let tool = ToolCatalog::global().get(name).ok_or((
                    INVALID_PARAMS,
                    format!("unknown tool {name:?}; use {SEARCH_TOOLS} to find tools"),
                ))?;
                self.call_rpc_tool(tool, arguments, context).await
            }
        };
        Ok(result.into_json(structured))
    }

    async fn call_rpc_tool(
        &self,
        tool: &ToolDef,
        mut arguments: Value,
        context: &CallContext,
    ) -> ToolResult {
        let mut limits = StreamLimits {
            max_items: DEFAULT_MAX_ITEMS as usize,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        };
        if tool.collects_stream() {
            let object = arguments.as_object_mut();
            let max_items = object.as_ref().and_then(|o| o.get("max_items")).cloned();
            let timeout_ms = object.as_ref().and_then(|o| o.get("timeout_ms")).cloned();
            if let Some(o) = object {
                o.remove("max_items");
                o.remove("timeout_ms");
            }
            match positive(max_items.as_ref(), "max_items") {
                Ok(Some(n)) => {
                    limits.max_items = n.min(self.inner.config.max_stream_items) as usize;
                }
                Ok(None) => {}
                Err(detail) => return ToolResult::failure(tool, &detail, None),
            }
            match positive(timeout_ms.as_ref(), "timeout_ms") {
                Ok(Some(n)) => {
                    limits.timeout =
                        Duration::from_millis(n.min(self.inner.config.max_stream_timeout_ms));
                }
                Ok(None) => {}
                Err(detail) => return ToolResult::failure(tool, &detail, None),
            }
        }
        if let Err(message) = normalize_repo_ref_arguments(tool.method, &mut arguments) {
            let detail = error_with_subcode(ErrorReason::InvalidArgument, "bad_repo_ref", message);
            return ToolResult::failure(tool, &detail, None);
        }
        let client = self
            .inner
            .client
            .with_authorization(context.authorization.clone());
        match client.call_method(tool.method, &arguments, limits).await {
            Ok(RpcOutput::Message(value)) => {
                ToolResult::success(format!("{}: ok", tool.title), value)
            }
            Ok(RpcOutput::Stream(stream)) => match &stream.error {
                Some(error) => ToolResult::failure(tool, error, Some(stream.items.len())),
                None => {
                    let summary = match stream.truncated {
                        None => format!(
                            "{}: {} message(s), complete",
                            tool.title,
                            stream.items.len()
                        ),
                        Some(Truncation::MaxItems) => format!(
                            "{}: {} message(s), truncated at max_items; raise max_items or page to see more",
                            tool.title,
                            stream.items.len()
                        ),
                        Some(Truncation::Timeout) => format!(
                            "{}: {} message(s), truncated at timeout_ms; the stream was still open",
                            tool.title,
                            stream.items.len()
                        ),
                    };
                    ToolResult::success(summary, stream.to_json())
                }
            },
            Err(error) => ToolResult::failure(tool, &error, None),
        }
    }

    fn search_tools(&self, session: &Session, arguments: &Value) -> ToolResult {
        let Some(query) = arguments.get("query").and_then(Value::as_str) else {
            return ToolResult::meta_failure(&error_with_subcode(
                ErrorReason::InvalidArgument,
                "missing_field",
                "search_tools needs a string `query`",
            ));
        };
        let limit = match positive(arguments.get("limit"), "limit") {
            Ok(n) => n.unwrap_or(20).min(100) as usize,
            Err(detail) => return ToolResult::meta_failure(&detail),
        };
        let state = session.state();
        let hits = ToolCatalog::global().search(query, limit);
        let matches: Vec<Value> = hits
            .iter()
            .map(|hit| {
                json!({
                    "name": hit.tool.name,
                    "description": hit.tool.summary(),
                    "toolsets": hit.tool.toolsets,
                    "listed": is_enabled(&state, hit.tool),
                })
            })
            .collect();
        drop(state);
        let mut text = format!("{} tool(s) match {query:?}", matches.len());
        for hit in &hits {
            text.push_str(&format!("\n- {}: {}", hit.tool.name, hit.tool.summary()));
        }
        if !hits.is_empty() {
            text.push_str(&format!(
                "\nCall any of them directly, or {ENABLE_TOOLSET} to list them."
            ));
        }
        ToolResult::with_text(text, json!({"query": query, "matches": matches}))
    }

    fn list_toolsets(&self, session: &Session) -> ToolResult {
        let catalog = ToolCatalog::global();
        let state = session.state();
        let mut text = String::from("Toolsets (enable with enable_toolset):");
        let toolsets: Vec<Value> = catalog
            .toolset_names()
            .into_iter()
            .map(|name| {
                let count = catalog.toolset_tools(name).count();
                let enabled = state.toolsets.contains(name);
                text.push_str(&format!(
                    "\n- {name} ({count} tools{}): {}",
                    if enabled { ", enabled" } else { "" },
                    catalog.toolset_description(name)
                ));
                json!({
                    "name": name,
                    "description": catalog.toolset_description(name),
                    "tools": count,
                    "enabled": enabled,
                })
            })
            .collect();
        ToolResult::with_text(text, json!({"toolsets": toolsets}))
    }

    fn enable_toolset(
        &self,
        session: &Session,
        arguments: &Value,
        notifications: &mut Vec<Value>,
    ) -> ToolResult {
        let catalog = ToolCatalog::global();
        let mut toolsets: Vec<String> = Vec::new();
        let mut tools: Vec<String> = Vec::new();
        let strings = |value: Option<&Value>, field: &str| -> Result<Vec<String>, ErrorDetail> {
            match value {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::String(s)) => Ok(vec![s.clone()]),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|v| {
                        v.as_str().map(str::to_owned).ok_or_else(|| {
                            error_with_subcode(
                                ErrorReason::InvalidArgument,
                                "bad_json",
                                format!("`{field}` must hold strings"),
                            )
                        })
                    })
                    .collect(),
                Some(_) => Err(error_with_subcode(
                    ErrorReason::InvalidArgument,
                    "bad_json",
                    format!("`{field}` must be a string or an array of strings"),
                )),
            }
        };
        for field in ["toolset", "toolsets", "tools"] {
            match strings(arguments.get(field), field) {
                Ok(values) if field == "tools" => tools.extend(values),
                Ok(values) => toolsets.extend(values),
                Err(detail) => return ToolResult::meta_failure(&detail),
            }
        }
        if toolsets.is_empty() && tools.is_empty() {
            return ToolResult::meta_failure(&error_with_subcode(
                ErrorReason::InvalidArgument,
                "missing_field",
                format!(
                    "pass `toolsets` and/or `tools`; toolsets are {}",
                    catalog.toolset_names().join(", ")
                ),
            ));
        }
        let unknown_sets: Vec<&String> =
            toolsets.iter().filter(|t| !catalog.is_toolset(t)).collect();
        let unknown_tools: Vec<&String> =
            tools.iter().filter(|t| catalog.get(t).is_none()).collect();
        if !unknown_sets.is_empty() || !unknown_tools.is_empty() {
            let mut detail = error_with_subcode(
                ErrorReason::NotFound,
                "unknown_toolset",
                format!(
                    "unknown toolset(s) {:?} / tool(s) {:?}; toolsets are {}",
                    unknown_sets,
                    unknown_tools,
                    catalog.toolset_names().join(", ")
                ),
            );
            detail.retryable = false;
            return ToolResult::meta_failure(&detail);
        }
        let before = self.listed_names(session).len();
        {
            let mut state = session.state();
            state.toolsets.extend(toolsets.iter().cloned());
            state.tools.extend(tools.iter().cloned());
        }
        let after = self.listed_names(session).len();
        let added = after - before;
        if added > 0 {
            notifications.push(notification("notifications/tools/list_changed", None));
        }
        let enabled = session.enabled_toolsets();
        ToolResult::with_text(
            format!(
                "{added} tool(s) added; {} listed. Enabled toolsets: {}.",
                after,
                enabled.join(", ")
            ),
            json!({"added": added, "listed": after, "toolsets": enabled}),
        )
    }
}

fn is_enabled(state: &SessionState, tool: &ToolDef) -> bool {
    state.toolsets.contains(ALL_TOOLSET)
        || state.tools.contains(&tool.name)
        || tool.toolsets.iter().any(|t| state.toolsets.contains(*t))
}

fn parse_cursor(cursor: &str) -> Option<usize> {
    cursor.strip_prefix("page:")?.parse().ok()
}

/// A non-negative integer argument that must be at least 1 when present.
fn positive(value: Option<&Value>, field: &str) -> Result<Option<u64>, ErrorDetail> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => Ok(Some(n)),
            _ => Err(error_with_subcode(
                ErrorReason::InvalidArgument,
                "bad_json",
                format!("`{field}` must be a positive integer"),
            )),
        },
    }
}

fn instructions(toolsets: &[String]) -> String {
    format!(
        "Tools call bee RPCs: arguments are the proto3-JSON request (lowerCamelCase fields, \
         64-bit integers as strings, bytes as base64) and results are the proto3-JSON \
         response. Failed calls return {{reason, message, retryable, payload}}; retry only \
         when retryable is true. Enabled toolsets: {}. Use {SEARCH_TOOLS} to find any of the \
         1,000+ bee tools by keyword, then call it directly or {ENABLE_TOOLSET} to list it; \
         {LIST_TOOLSETS} shows every toolset.",
        toolsets.join(", ")
    )
}

fn meta_tool(name: &str, structured: bool) -> Option<Value> {
    let (title, description, schema) = match name {
        SEARCH_TOOLS => (
            "Search bee tools",
            "Search all 1,000+ bee tools (every RPC) by keyword, including tools not in the \
             current list. Returns names, one-line descriptions, toolsets and whether each is \
             listed. Any returned tool can be called directly.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Keywords, e.g. \"merge queue\" or \"create issue\"."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Most matches to return (default 20)."}
                },
                "required": ["query"]
            }),
        ),
        LIST_TOOLSETS => (
            "List bee toolsets",
            "List every toolset (a group of related tools) with its tool count and whether it \
             is enabled in this session.",
            json!({"type": "object", "properties": {}}),
        ),
        ENABLE_TOOLSET => (
            "Enable bee toolsets",
            "Add toolsets (see list_toolsets) or single tools to this session's tool list. \
             The server then sends notifications/tools/list_changed.",
            json!({
                "type": "object",
                "properties": {
                    "toolsets": {"type": "array", "items": {"type": "string"}, "description": "Toolset names, e.g. [\"ci\", \"security\"]; \"all\" lists every tool."},
                    "toolset": {"type": "string", "description": "One toolset name."},
                    "tools": {"type": "array", "items": {"type": "string"}, "description": "Single tool names, e.g. [\"workflow_list_workflow_runs\"]."}
                }
            }),
        ),
        _ => return None,
    };
    let mut tool = json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "annotations": {"readOnlyHint": name != ENABLE_TOOLSET, "openWorldHint": false},
    });
    if structured && let Some(object) = tool.as_object_mut() {
        object.insert("title".into(), json!(title));
    }
    Some(tool)
}

/// One tool call's outcome before rendering for a protocol version.
struct ToolResult {
    text: String,
    structured: Value,
    is_error: bool,
}

impl ToolResult {
    fn success(summary: String, value: Value) -> Self {
        let json = serde_json::to_string(&value).unwrap_or_default();
        Self {
            text: format!("{summary}\n{json}"),
            structured: value,
            is_error: false,
        }
    }

    fn with_text(text: String, value: Value) -> Self {
        Self {
            text,
            structured: value,
            is_error: false,
        }
    }

    fn failure(tool: &ToolDef, detail: &ErrorDetail, items_received: Option<usize>) -> Self {
        let doc = error_document(detail);
        let retry = if detail.retryable {
            "retryable"
        } else {
            "not retryable"
        };
        let mut text = format!(
            "{} failed: {} ({retry}): {}",
            tool.title, detail.reason, detail.message
        );
        if let Some(n) = items_received {
            text.push_str(&format!(" [after {n} stream message(s)]"));
        }
        text.push('\n');
        text.push_str(&serde_json::to_string(&doc).unwrap_or_default());
        Self {
            text,
            structured: doc,
            is_error: true,
        }
    }

    fn meta_failure(detail: &ErrorDetail) -> Self {
        let doc = error_document(detail);
        Self {
            text: format!("{}: {}", detail.reason, detail.message),
            structured: doc,
            is_error: true,
        }
    }

    fn into_json(self, structured: bool) -> Value {
        let mut text = self.text;
        if text.len() > MAX_TEXT_BYTES {
            let mut cut = MAX_TEXT_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("\n[text truncated; the full result is in structuredContent]");
        }
        let mut result = Map::new();
        result.insert("content".into(), json!([{"type": "text", "text": text}]));
        if structured {
            result.insert("structuredContent".into(), self.structured);
        }
        result.insert("isError".into(), json!(self.is_error));
        Value::Object(result)
    }
}
