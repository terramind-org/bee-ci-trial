//! MCP server exposing every bee operation as a tool (SPEC §12.4, PRODUCT §4.8).
//!
//! Every `bee.v1` RPC is an MCP tool named `<service>_<method>` (`write_land`,
//! `query_log`, `change_open_change`, `issue_create_issue`) whose input schema is the request
//! message's JSON Schema and whose result is the response message; see [`catalog`]. Tools call
//! a bee server over its HTTP/JSON surface through [`client::RpcClient::call_rpc_json`], the
//! same code path `bee-cli` uses, so the API, the CLI and MCP behave identically
//! (PRODUCT §3.2). Any server that speaks the Connect-shaped surface of SPEC §12.1 works.
//!
//! # Transports
//!
//! - **stdio** for `bee mcp`: [`run_stdio`] (or [`stdio::serve_stdio`] over any streams).
//! - **Streamable HTTP** for `/mcp` on `bee-server`: [`mcp_router`], mounted with
//!   `Router::new().nest("/mcp", bee_mcp::mcp_router(config)?)`.
//!
//! Both speak MCP protocol revisions 2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05
//! (handshake-based: `initialize`, then `notifications/initialized`), with `ping`,
//! paginated `tools/list`, `tools/call` and `notifications/tools/list_changed`.
//!
//! # Keeping 1,000+ tools usable
//!
//! Tools are grouped into toolsets ([`catalog::TOOLSETS`]); a session lists only its enabled
//! toolsets, by default `core` (objects, queries, writes and lands, refs, change requests,
//! checks, leases, events, issues, repos, and `auth_who_am_i`). Configure others with
//! [`McpConfig::toolsets`] or `BEE_MCP_TOOLSETS=core,ci,security` (`all` lists everything).
//! At run time an agent uses the `search_tools` meta-tool to find any tool by keyword (and may
//! call it directly), `list_toolsets` to see the groups, and `enable_toolset` to add groups or
//! single tools to its list, after which the server sends
//! `notifications/tools/list_changed`. See [`server`] for the full behaviour.
//!
//! # Configuration
//!
//! [`McpConfig::from_env`] reads `BEE_ENDPOINT` (default `http://127.0.0.1:7420`),
//! `BEE_TOKEN` (sent as `Authorization: Bearer`), and `BEE_MCP_TOOLSETS`. Over HTTP each
//! caller's own `Authorization` header is forwarded instead.

// `ErrorDetail` is the SPEC §12.2 contract type and is returned by value on purpose: errors are
// the cold path, and boxing would make every caller unwrap a box to read the contract.
#![allow(clippy::result_large_err, clippy::large_enum_variant)]

pub mod catalog;
pub mod client;
pub mod http;
pub mod protocol;
pub mod server;
pub mod stdio;

pub use client::{
    ClientConfig, RpcClient, RpcOutput, StreamLimits, StreamOutput, call_rpc_json, error_document,
};
pub use http::mcp_router;
pub use server::{ConfigError, McpConfig, McpServer};
pub use stdio::run_stdio;
