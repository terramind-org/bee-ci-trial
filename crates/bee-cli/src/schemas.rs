//! The published `--json` output schemas: one file per command under `schemas/v1/cli/`,
//! written by `bee-cli-gen` and checked by a test.
//!
//! A generated command's schema is the envelope `{"schema": <const>, "data": <response>}` whose
//! `data` refers to the response message schema in `schemas/v1/messages/` by its `$id`. A
//! registered command's schema embeds its own data schema.

use std::collections::BTreeMap;

use bee_proto::methods::MethodKind;
use bee_proto::schema::{MESSAGES_DIR, SCHEMA_BASE_URI, render};
use serde_json::{Value, json};

use crate::catalog::{Noun, Verb, catalog};
use crate::commands::{Command, registry, schema_of};
use crate::output::schema_id;

/// Directory of the CLI output schemas, relative to the repository root.
pub const CLI_SCHEMA_DIR: &str = "schemas/v1/cli";

/// Path of the error document schema, relative to the repository root.
pub const ERROR_SCHEMA_PATH: &str = "schemas/v1/error.schema.json";

const ERROR_SCHEMA: &str = include_str!("../../../schemas/v1/error.schema.json");

/// The error document schema.
pub fn error_schema() -> Value {
    serde_json::from_str(ERROR_SCHEMA).unwrap_or(Value::Null)
}

/// `schemas/v1/messages/<message>.schema.json`.
pub fn message_path(message: &str) -> String {
    format!("{MESSAGES_DIR}/{message}.schema.json")
}

/// The published URI (`$id`) of a message schema.
pub fn message_uri(message: &str) -> String {
    format!("{SCHEMA_BASE_URI}{}", message_path(message))
}

/// `schemas/v1/cli/<path joined by .>.schema.json`.
pub fn cli_schema_path(path: &[&str]) -> String {
    format!("{CLI_SCHEMA_DIR}/{}.schema.json", path.join("."))
}

fn envelope_schema(path: &[&str], description: String, data: Value) -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": format!("{SCHEMA_BASE_URI}{}", cli_schema_path(path)),
        "title": format!("bee {}", path.join(" ")),
        "description": description,
        "type": "object",
        "required": ["schema", "data"],
        "additionalProperties": false,
        "properties": {
            "schema": {"const": schema_id(path)},
            "data": data
        }
    })
}

/// The output schema of a generated command.
pub fn rpc_output_schema(noun: &Noun, verb: &Verb) -> Value {
    let path = [noun.name.as_str(), verb.name.as_str()];
    let shape = match verb.method.kind {
        MethodKind::Unary | MethodKind::ClientStreaming => {
            format!("`bee {} --json` prints one such document.", path.join(" "))
        }
        MethodKind::ServerStreaming | MethodKind::BidiStreaming => format!(
            "`bee {} --json` prints one such document per streamed message, one per line \
             (NDJSON); a failure ends the stream with an error document.",
            path.join(" ")
        ),
    };
    let description = format!(
        "Output of RPC {}: `data` is a {} in proto3-JSON. {shape} Errors print a document \
         matching {ERROR_SCHEMA_PATH}.",
        verb.method.path, verb.method.output_type
    );
    envelope_schema(
        &path,
        description,
        json!({"$ref": message_uri(verb.method.output_type)}),
    )
}

/// The output schema of a registered command.
pub fn registered_output_schema(command: &dyn Command) -> Value {
    let shape = if command.streaming() {
        "one such document per item, one per line (NDJSON)"
    } else {
        "one such document"
    };
    let description = format!(
        "Output of `bee {} --json`: {shape} with `schema` = {}. {} Errors print a document \
         matching {ERROR_SCHEMA_PATH}.",
        command.path().join(" "),
        schema_of(command),
        command.about()
    );
    envelope_schema(command.path(), description, command.output_schema())
}

/// Every generated schema file, keyed by its path relative to the repository root.
pub fn generated_files() -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for (noun, verb) in catalog().commands() {
        files.insert(
            cli_schema_path(&[&noun.name, &verb.name]),
            render(&rpc_output_schema(noun, verb)),
        );
    }
    for command in registry() {
        files.insert(
            cli_schema_path(command.path()),
            render(&registered_output_schema(command)),
        );
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_schema_is_embedded() {
        assert_eq!(
            error_schema()["properties"]["schema"]["const"],
            "bee.v1.error"
        );
    }

    #[test]
    fn rpc_schema_refers_to_the_response_message() {
        let noun = catalog().noun("write").expect("write");
        let verb = noun.verb("land").expect("land");
        let schema = rpc_output_schema(noun, verb);
        assert_eq!(schema["properties"]["schema"]["const"], "bee.v1.write.land");
        assert_eq!(
            schema["properties"]["data"]["$ref"],
            "https://bee.dev/schemas/v1/messages/bee.v1.LandResponse.schema.json"
        );
    }
}
