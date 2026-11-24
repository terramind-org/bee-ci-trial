//! `bee mcp`: every RPC as an MCP tool over stdio (SPEC §12.4), for agent runtimes that
//! launch MCP servers as child processes.

use clap::{Arg, ArgMatches};
use serde_json::{Value, json};

use super::Command;
use crate::context::Context;
use crate::error::CliError;
use crate::rpc::Endpoint;

/// `bee mcp`: serves MCP on stdin/stdout until stdin closes.
pub struct Mcp;

impl Command for Mcp {
    fn path(&self) -> &'static [&'static str] {
        &["mcp"]
    }

    fn about(&self) -> &'static str {
        "Serve every bee RPC as an MCP tool over stdio (for agent runtimes)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Serve the Model Context Protocol on stdin/stdout: one tool per RPC, named \
                 <service>_<method> (query_log, write_land, issue_create_issue), whose input \
                 schema is the request message's JSON Schema. Tools call the server at \
                 --endpoint / BEE_ENDPOINT with --token / BEE_TOKEN (or `bee login`), the \
                 same as every other command. Sessions list the `core` toolset unless \
                 --toolsets or BEE_MCP_TOOLSETS names others (`all` lists every tool); the \
                 search_tools, list_toolsets and enable_toolset meta-tools reach the rest at run \
                 time. stdout carries only MCP messages, so --json prints no document; the \
                 command returns when stdin closes.",
            )
            .arg(
                Arg::new("toolsets")
                    .long("toolsets")
                    .value_name("LIST")
                    .help("Comma-separated toolsets each session starts with (default core)"),
            )
            .after_help(
                "EXAMPLE:\n  bee mcp --endpoint http://127.0.0.1:7420 --toolsets core,ci\n  \
                 claude mcp add bee -- bee mcp",
            )
    }

    fn output_schema(&self) -> Value {
        json!({
            "description": "bee mcp prints no --json document: stdout carries MCP JSON-RPC \
                            messages, one per line, until stdin closes.",
            "not": {}
        })
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let endpoint = match ctx.endpoint()? {
            Endpoint::Http { host, port, prefix } => format!("http://{host}:{port}{prefix}"),
            Endpoint::Unix(path) => {
                return Err(CliError::usage(
                    "unsupported_endpoint",
                    format!(
                        "bee mcp needs an http:// endpoint, not unix:{}",
                        path.display()
                    ),
                ));
            }
        };
        let mut config = bee_mcp::McpConfig::from_env();
        config.client.endpoint = endpoint;
        config.client.token = ctx.token()?;
        if let Some(timeout) = ctx.options.timeout {
            config.client.request_timeout = timeout;
        }
        if let Some(list) = args.get_one::<String>("toolsets") {
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
        let server = bee_mcp::McpServer::new(config)
            .map_err(|e| CliError::usage("unknown_toolset", e.to_string()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::internal(format!("cannot start the I/O runtime: {e}")))?;
        runtime
            .block_on(bee_mcp::stdio::serve_stdio(
                &server,
                tokio::io::stdin(),
                tokio::io::stdout(),
            ))
            .map_err(|e| CliError::unavailable(format!("bee mcp: {e}")))?;
        Ok(None)
    }
}
