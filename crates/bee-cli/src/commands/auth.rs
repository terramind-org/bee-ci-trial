//! `bee login`, `bee logout` and `bee whoami`.

use bee_proto::methods::find_method;
use clap::ArgMatches;
use serde_json::{Value, json};

use super::{Command, message_ref};
use crate::catalog::catalog;
use crate::context::{Context, ENV_TOKEN};
use crate::credentials::{self, Credentials};
use crate::error::CliError;
use crate::output;

/// `bee login`: stores a token (and optionally an endpoint) in the credentials file.
pub struct Login;

impl Command for Login {
    fn path(&self) -> &'static [&'static str] {
        &["login"]
    }

    fn about(&self) -> &'static str {
        "Store a token (and default endpoint) in ~/.config/bee/credentials (mode 0600)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Store a bearer token, and the --endpoint if given, in the credentials file \
                 (~/.config/bee/credentials, or $BEE_CONFIG_DIR/credentials), created with \
                 mode 0600. Later commands use them when --token/BEE_TOKEN and \
                 --endpoint/BEE_ENDPOINT are not set. Nothing is sent to the server; run \
                 `bee whoami` to check the token.\n\nThe token comes from the global --token \
                 flag (`--token -` reads it from stdin) or BEE_TOKEN.",
            )
            .after_help(
                "EXAMPLE:\n  bee login --token \"$TOKEN\" --endpoint http://127.0.0.1:8080 --json\n  \
                 printf %s \"$TOKEN\" | bee login --token -",
            )
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["credentialsPath", "endpoint"],
            "additionalProperties": false,
            "properties": {
                "credentialsPath": {"type": "string", "description": "File the token was written to."},
                "endpoint": {"type": ["string", "null"], "description": "Default endpoint stored with it, if any."}
            }
        })
    }

    fn run(&self, ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let token = match ctx.options.token.clone() {
            Some(token) if token == "-" => ctx.read_stdin()?.trim().to_owned(),
            Some(token) => token,
            None => std::env::var(ENV_TOKEN).unwrap_or_default(),
        };
        if token.trim().is_empty() {
            return Err(CliError::usage(
                "missing_token",
                "bee login needs a token: --token <token>, --token - (stdin) or BEE_TOKEN",
            ));
        }
        let endpoint = ctx.options.endpoint.clone();
        if let Some(endpoint) = &endpoint {
            crate::rpc::Endpoint::parse(endpoint)?;
        }
        let path = credentials::store(&Credentials {
            token: token.trim().to_owned(),
            endpoint: endpoint.clone(),
        })?;
        Ok(Some(json!({
            "credentialsPath": path.display().to_string(),
            "endpoint": endpoint,
        })))
    }

    fn render(&self, data: &Value) -> String {
        let mut text = format!(
            "Token stored in {}",
            data["credentialsPath"].as_str().unwrap_or_default()
        );
        if let Some(endpoint) = data["endpoint"].as_str() {
            text.push_str(&format!(" (endpoint {endpoint})"));
        }
        text
    }
}

/// `bee logout`: deletes the credentials file.
pub struct Logout;

impl Command for Logout {
    fn path(&self) -> &'static [&'static str] {
        &["logout"]
    }

    fn about(&self) -> &'static str {
        "Delete the stored credentials."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command.after_help("EXAMPLE:\n  bee logout --json")
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["credentialsPath", "removed"],
            "additionalProperties": false,
            "properties": {
                "credentialsPath": {"type": "string"},
                "removed": {"type": "boolean", "description": "False if there was nothing to remove."}
            }
        })
    }

    fn run(&self, _ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let (path, removed) = credentials::remove()?;
        Ok(Some(json!({
            "credentialsPath": path.display().to_string(),
            "removed": removed,
        })))
    }

    fn render(&self, data: &Value) -> String {
        let path = data["credentialsPath"].as_str().unwrap_or_default();
        if data["removed"].as_bool().unwrap_or(false) {
            format!("Removed {path}")
        } else {
            format!("No credentials at {path}")
        }
    }
}

/// `bee whoami`: calls `AuthService/WhoAmI`.
pub struct WhoAmI;

const WHO_AM_I: &str = "/bee.v1.AuthService/WhoAmI";

impl Command for WhoAmI {
    fn path(&self) -> &'static [&'static str] {
        &["whoami"]
    }

    fn about(&self) -> &'static str {
        "Show the principal, scope and budget of the current token (AuthService/WhoAmI)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command.after_help(
            "EXAMPLE:\n  bee whoami --json\n\nSame call as `bee auth who-am-i`; output data is \
             a bee.v1.WhoAmIResponse.",
        )
    }

    fn output_schema(&self) -> Value {
        message_ref("bee.v1.WhoAmIResponse")
    }

    fn run(&self, ctx: &mut Context, _args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let method = find_method(WHO_AM_I)
            .ok_or_else(|| CliError::internal(format!("{WHO_AM_I} is not in the catalog")))?;
        ctx.call_unary(method, &json!({})).map(Some)
    }

    fn render(&self, data: &Value) -> String {
        output::render_message(catalog(), "bee.v1.WhoAmIResponse", data)
    }
}
