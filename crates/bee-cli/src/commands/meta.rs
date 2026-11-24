//! `bee schema` and `bee help-agent`: the CLI describing itself to agents.

use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Value, json};

use super::{Command, registry, schema_of};
use crate::catalog::catalog;
use crate::cli::GLOBAL_FLAGS;
use crate::error::{CliError, ExitCode};
use crate::schemas;

/// `bee schema <noun> <verb>`: request, response and output schemas of a command.
pub struct Schema;

/// What `bee schema` knows about one command.
fn describe(words: &[&str]) -> Result<Value, CliError> {
    let catalog = catalog();
    if words == ["error"] {
        return Ok(json!({
            "command": "error",
            "schema": "bee.v1.error",
            "kind": "error",
            "rpc": null,
            "request": null,
            "response": null,
            "output": schemas::error_schema(),
        }));
    }
    if let Some(command) = registry().into_iter().find(|c| c.path() == words) {
        return Ok(json!({
            "command": words.join(" "),
            "schema": schema_of(command),
            "kind": if command.streaming() { "stream" } else { "local" },
            "rpc": null,
            "request": null,
            "response": null,
            "output": schemas::registered_output_schema(command),
        }));
    }
    if let [noun_name, verb_name] = words
        && let Some(noun) = catalog.noun(noun_name)
        && let Some(verb) = noun.verb(verb_name)
    {
        let messages = bee_proto::schema::Catalog::load();
        return Ok(json!({
            "command": words.join(" "),
            "schema": crate::output::schema_id(words),
            "kind": verb.shape(),
            "rpc": verb.method.path,
            "request": messages.message_schema(verb.method.input_type),
            "response": messages.message_schema(verb.method.output_type),
            "output": schemas::rpc_output_schema(noun, verb),
        }));
    }
    Err(CliError::usage(
        "unknown_command",
        format!(
            "no command `bee {}`; run `bee help-agent` for the list",
            words.join(" ")
        ),
    ))
}

impl Command for Schema {
    fn path(&self) -> &'static [&'static str] {
        &["schema"]
    }

    fn about(&self) -> &'static str {
        "Print the request, response and --json output JSON Schemas of a command."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(
                Arg::new("command")
                    .value_name("COMMAND")
                    .num_args(1..=2)
                    .required(true)
                    .action(ArgAction::Set)
                    .help(
                        "The command: `<noun> <verb>` (e.g. `write land`), a built-in such as \
                         `whoami`, or `error` for the error document",
                    ),
            )
            .after_help(
                "EXAMPLE:\n  bee schema write land\n  bee schema change open-change --json\n\n\
                 `request` and `response` are the proto3-JSON message schemas \
                 (schemas/v1/messages/); `output` is the --json envelope \
                 (schemas/v1/cli/<noun>.<verb>.schema.json).",
            )
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["command", "schema", "kind", "rpc", "request", "response", "output"],
            "additionalProperties": false,
            "properties": {
                "command": {"type": "string", "description": "Command words, e.g. `write land`."},
                "schema": {"type": "string", "description": "The `schema` value of its --json output."},
                "kind": {"type": "string", "enum": ["unary", "server-streaming", "client-streaming", "bidi-streaming", "local", "stream", "error"]},
                "rpc": {"type": ["string", "null"], "description": "RPC path, for generated commands."},
                "request": {"type": ["object", "null"], "description": "JSON Schema of the request message."},
                "response": {"type": ["object", "null"], "description": "JSON Schema of the response message."},
                "output": {"type": "object", "description": "JSON Schema of the --json output document."}
            }
        })
    }

    fn run(
        &self,
        _ctx: &mut crate::context::Context,
        args: &ArgMatches,
    ) -> Result<Option<Value>, CliError> {
        let words: Vec<&str> = args
            .get_many::<String>("command")
            .map(|values| values.map(String::as_str).collect())
            .unwrap_or_default();
        describe(&words).map(Some)
    }

    fn render(&self, data: &Value) -> String {
        serde_json::to_string_pretty(data).unwrap_or_default()
    }
}

/// `bee help-agent`: a compact index of every command for agents.
pub struct HelpAgent;

fn index() -> Value {
    let catalog = catalog();
    let mut commands = Vec::new();
    for command in registry() {
        commands.push(json!({
            "command": command.path().join(" "),
            "kind": if command.streaming() { "stream" } else { "local" },
            "rpc": null,
            "request": null,
            "output": schemas::cli_schema_path(command.path()),
            "summary": command.about(),
        }));
    }
    for (noun, verb) in catalog.commands() {
        commands.push(json!({
            "command": format!("{} {}", noun.name, verb.name),
            "kind": verb.shape(),
            "rpc": verb.method.path,
            "request": schemas::message_path(verb.method.input_type),
            "output": schemas::cli_schema_path(&[&noun.name, &verb.name]),
            "summary": verb.summary(),
        }));
    }
    let global_flags: Vec<Value> = GLOBAL_FLAGS
        .iter()
        .map(|(flag, env, help)| json!({"flag": flag, "env": env, "description": help}))
        .collect();
    let exit_codes: Vec<Value> = ExitCode::TABLE
        .iter()
        .map(|(code, meaning)| json!({"code": code.code(), "meaning": meaning}))
        .collect();
    json!({
        "usage": "bee <noun> <verb> [--field value ...] [--json]",
        "flags": "Request fields are flags (--field-name value); repeated fields repeat the flag; \
                  messages take JSON or a shorthand; --json-input '<json>' or --input-file <path|-> \
                  supply the whole request, flags override.",
        "globalFlags": global_flags,
        "exitCodes": exit_codes,
        "schemas": {
            "error": schemas::ERROR_SCHEMA_PATH,
        },
        "commands": commands,
    })
}

impl Command for HelpAgent {
    fn path(&self) -> &'static [&'static str] {
        &["help-agent"]
    }

    fn about(&self) -> &'static str {
        "Print a compact index of every command, its RPC and schema paths, for agents."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command.after_help(
            "EXAMPLE:\n  bee help-agent\n  bee help-agent --json\n\nText form: one line per \
             command: `<command> | <kind> | <request schema> | <summary>`. Then run \
             `bee <noun> <verb> --help` for its flags.",
        )
    }

    fn output_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["usage", "flags", "globalFlags", "exitCodes", "schemas", "commands"],
            "additionalProperties": false,
            "properties": {
                "usage": {"type": "string"},
                "flags": {"type": "string"},
                "globalFlags": {"type": "array", "items": {
                    "type": "object",
                    "required": ["flag", "env", "description"],
                    "additionalProperties": false,
                    "properties": {
                        "flag": {"type": "string"},
                        "env": {"type": ["string", "null"]},
                        "description": {"type": "string"}
                    }
                }},
                "exitCodes": {"type": "array", "items": {
                    "type": "object",
                    "required": ["code", "meaning"],
                    "additionalProperties": false,
                    "properties": {"code": {"type": "integer"}, "meaning": {"type": "string"}}
                }},
                "schemas": {"type": "object", "additionalProperties": {"type": "string"}},
                "commands": {"type": "array", "items": {
                    "type": "object",
                    "required": ["command", "kind", "rpc", "request", "output", "summary"],
                    "additionalProperties": false,
                    "properties": {
                        "command": {"type": "string"},
                        "kind": {"type": "string"},
                        "rpc": {"type": ["string", "null"]},
                        "request": {"type": ["string", "null"], "description": "Request message schema path."},
                        "output": {"type": "string", "description": "--json output schema path."},
                        "summary": {"type": "string"}
                    }
                }}
            }
        })
    }

    fn run(
        &self,
        _ctx: &mut crate::context::Context,
        _args: &ArgMatches,
    ) -> Result<Option<Value>, CliError> {
        Ok(Some(index()))
    }

    fn render(&self, data: &Value) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "bee agent index. Usage: {}\n{}\n",
            data["usage"].as_str().unwrap_or_default(),
            data["flags"].as_str().unwrap_or_default()
        ));
        out.push_str("Global:");
        for flag in data["globalFlags"].as_array().into_iter().flatten() {
            out.push_str(&format!(" {}", flag["flag"].as_str().unwrap_or_default()));
            if let Some(env) = flag["env"].as_str() {
                out.push_str(&format!(" (${env})"));
            }
        }
        out.push_str("\nExit:");
        for code in data["exitCodes"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                " {}={}",
                code["code"],
                code["meaning"].as_str().unwrap_or_default()
            ));
        }
        out.push_str(&format!(
            "\nOutput schema of `bee <noun> <verb>`: schemas/v1/cli/<noun>.<verb>.schema.json; \
             errors: {}\n",
            schemas::ERROR_SCHEMA_PATH
        ));
        for command in data["commands"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "{} | {} | {} | {}\n",
                command["command"].as_str().unwrap_or_default(),
                command["kind"].as_str().unwrap_or_default(),
                command["request"].as_str().unwrap_or("-"),
                command["summary"].as_str().unwrap_or_default()
            ));
        }
        out.trim_end().to_owned()
    }
}
