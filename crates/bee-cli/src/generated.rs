//! Running a generated `bee <noun> <verb>`: flags and JSON input become the request, the RPC
//! is called, and the response is printed (one document, or NDJSON for streams).

use std::time::Duration;

use bee_proto::methods::MethodKind;
use clap::ArgMatches;
use serde_json::{Map, Value};

use crate::catalog::{Catalog, Noun, Verb};
use crate::cli::follow_is_field;
use crate::context::Context;
use crate::error::CliError;
use crate::fields::{check_known_fields, field_flags, field_value, set_field};
use crate::output::{render_message, schema_id};
use crate::rpc::StreamMode;

/// How long an event stream without `--follow` waits for the next event before stopping.
pub const CATCH_UP_IDLE: Duration = Duration::from_secs(2);

/// Parses JSON input: one object, or (for client streams) an array or a sequence of objects.
fn parse_input(text: &str, many: bool, source: &str) -> Result<Vec<Value>, CliError> {
    let bad = |message: String| CliError::usage("bad_input", format!("{source}: {message}"));
    let mut values = Vec::new();
    for value in serde_json::Deserializer::from_str(text).into_iter::<Value>() {
        values.push(value.map_err(|e| bad(format!("invalid JSON: {e}")))?);
    }
    if many {
        if let [Value::Array(items)] = values.as_slice() {
            values = items.clone();
        }
    } else if values.len() != 1 {
        return Err(bad(format!(
            "expected one JSON object, found {} JSON values",
            values.len()
        )));
    }
    for value in &values {
        if !value.is_object() {
            return Err(bad("every message must be a JSON object".into()));
        }
    }
    Ok(values)
}

/// The request messages of an invocation, in canonical proto3-JSON.
pub fn build_requests(
    ctx: &Context,
    catalog: &Catalog,
    verb: &Verb,
    args: &ArgMatches,
) -> Result<Vec<Value>, CliError> {
    let method = verb.method;
    let many = matches!(
        method.kind,
        MethodKind::ClientStreaming | MethodKind::BidiStreaming
    );
    let message = catalog.message(method.input_type).ok_or_else(|| {
        CliError::internal(format!("{} is not in the catalog", method.input_type))
    })?;

    let mut from_flags = Map::new();
    for flag in field_flags(message, follow_is_field(catalog, verb)) {
        let Some(raw) = args.get_many::<String>(&flag.id) else {
            continue;
        };
        let raw: Vec<String> = raw.cloned().collect();
        let value = field_value(catalog, &flag, &raw)?;
        set_field(&mut from_flags, flag.field, value);
    }

    let mut inputs = Vec::new();
    if let Some(path) = args.get_one::<String>("input-file") {
        let (text, source) = if path == "-" {
            (ctx.read_stdin()?, "stdin".to_owned())
        } else {
            let text = std::fs::read_to_string(path)
                .map_err(|e| CliError::usage("bad_input", format!("--input-file {path}: {e}")))?;
            (text, format!("--input-file {path}"))
        };
        inputs.extend(parse_input(&text, many, &source)?);
    }
    if let Some(text) = args.get_one::<String>("json-input") {
        inputs.extend(parse_input(text, many, "--json-input")?);
    }

    let messages = if many {
        if inputs.is_empty() {
            vec![Value::Object(from_flags)]
        } else if from_flags.is_empty() {
            inputs
        } else {
            return Err(CliError::usage(
                "bad_input",
                "give request messages either as flags (one message) or as JSON input \
                 (many), not both",
            ));
        }
    } else {
        let mut request = Map::new();
        for input in inputs {
            if let Value::Object(fields) = input {
                for (key, value) in fields {
                    if let Some(field) = message.field(&key) {
                        set_field(&mut request, field, value);
                    } else {
                        request.insert(key, value);
                    }
                }
            }
        }
        for (key, value) in from_flags {
            if let Some(field) = message.field(&key) {
                set_field(&mut request, field, value);
            }
        }
        vec![Value::Object(request)]
    };

    messages
        .into_iter()
        .map(|request| {
            check_known_fields(catalog, method.input_type, &request, "")?;
            canonical(verb, &request)
        })
        .collect()
}

/// Round-trips a request through the protobuf encoding: rejects values of the wrong type and
/// normalises names, enums and integers to proto3-JSON.
fn canonical(verb: &Verb, request: &Value) -> Result<Value, CliError> {
    let method = verb.method;
    let invalid = |e: &dyn std::fmt::Display| {
        CliError::usage(
            "bad_request",
            format!("not a valid {}: {e}", method.input_type),
        )
    };
    let json = serde_json::to_vec(request).map_err(|e| invalid(&e))?;
    let proto = (method.request_json_to_proto)(&json).map_err(|e| invalid(&e))?;
    let back = (method.request_proto_to_json)(&proto).map_err(|e| invalid(&e))?;
    serde_json::from_slice(&back).map_err(|e| invalid(&e))
}

/// Runs a generated command.
pub fn run(
    ctx: &mut Context,
    catalog: &Catalog,
    noun: &Noun,
    verb: &Verb,
    args: &ArgMatches,
) -> Result<(), CliError> {
    let method = verb.method;
    let schema = schema_id(&[&noun.name, &verb.name]);
    let requests = build_requests(ctx, catalog, verb, args)?;
    let render = |data: &Value| render_message(catalog, method.output_type, data);
    match method.kind {
        MethodKind::Unary => {
            let request = requests.first().cloned().unwrap_or(Value::Null);
            let data = ctx.call_unary(method, &request)?;
            let human = render(&data);
            ctx.print_document(&schema, data, &human)
        }
        MethodKind::ClientStreaming => {
            let mut response = None;
            ctx.call_stream(method, &requests, StreamMode::ToEnd, &mut |item| {
                response = Some(item);
                Ok(())
            })?;
            let data = response.ok_or_else(|| {
                CliError::internal(format!("server sent no {}", method.output_type))
            })?;
            let human = render(&data);
            ctx.print_document(&schema, data, &human)
        }
        MethodKind::ServerStreaming | MethodKind::BidiStreaming => {
            let follow = if follow_is_field(catalog, verb) {
                requests
                    .first()
                    .and_then(|r| r.get("follow"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            } else {
                args.try_get_one::<bool>("follow")
                    .ok()
                    .flatten()
                    .copied()
                    .unwrap_or(false)
            };
            let mode = if verb.is_event_stream() && !follow {
                StreamMode::UntilIdle(CATCH_UP_IDLE)
            } else {
                StreamMode::ToEnd
            };
            let printer: &Context = ctx;
            printer.call_stream(method, &requests, mode, &mut |item| {
                let human = render(&item);
                printer.emit(&schema, item, &human)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_forms() {
        assert_eq!(parse_input("{\"a\":1}", false, "x").expect("one").len(), 1);
        assert!(parse_input("{} {}", false, "x").is_err());
        assert!(parse_input("[1]", false, "x").is_err());
        assert_eq!(parse_input("[{}, {}]", true, "x").expect("array").len(), 2);
        assert_eq!(
            parse_input("{}\n{}\n{}", true, "x").expect("ndjson").len(),
            3
        );
        assert!(parse_input("{", true, "x").is_err());
    }
}
