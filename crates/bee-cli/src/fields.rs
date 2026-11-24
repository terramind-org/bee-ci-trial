//! Request fields as flags: one `--field-name` per field of the request message, and the
//! conversion of flag values to proto3-JSON.
//!
//! - Scalars take their value: `--ref main`, `--limit 10`; 64-bit integers become JSON strings.
//! - `bool` fields are switches: `--allow-empty` or `--allow-empty=false`.
//! - `bytes` take UTF-8 text (`--path src/lib.rs`), `base64:<data>`, or `@<file>` for a file's
//!   contents; `text:<value>` passes a value starting with `base64:` or `@` literally.
//! - Enums take a value name, short (`squash`) or full (`LAND_METHOD_SQUASH`).
//! - Messages take a JSON object, or a shorthand: `org/name` or an id for a `RepoRef`, the text
//!   form of an `ObjectId`/`ChangeId`, or the value of a message's only field.
//! - Repeated fields repeat the flag; a repeated message also accepts one JSON array.
//! - Maps take `key=value`, repeated.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use crate::catalog::{Cardinality, Catalog, Field, Kind, MessageType, Scalar};
use crate::error::CliError;

/// Flag names the CLI itself uses on every verb; a field with one of these names gets the flag
/// `--<name>-field` instead.
pub const RESERVED: &[&str] = &[
    "help",
    "json",
    "endpoint",
    "token",
    "timeout",
    "json-input",
    "input-file",
    "follow",
];

/// A field and the flag that sets it.
#[derive(Clone, Debug)]
pub struct FieldFlag<'a> {
    /// The field.
    pub field: &'a Field,
    /// Long flag without dashes, e.g. `change-request-id`.
    pub long: String,
    /// clap argument id (distinct from every global argument id).
    pub id: String,
}

/// The flags of a request message, in field order. A `bool follow` field on an event stream
/// keeps the plain `--follow` flag (see [`crate::cli`]).
pub fn field_flags<'a>(message: &'a MessageType, follow_is_field: bool) -> Vec<FieldFlag<'a>> {
    message
        .fields
        .iter()
        .map(|field| {
            let base = field.name.replace('_', "-");
            let long =
                if RESERVED.contains(&base.as_str()) && !(follow_is_field && base == "follow") {
                    format!("{base}-field")
                } else {
                    base
                };
            FieldFlag {
                id: format!("field:{}", field.json_name),
                field,
                long,
            }
        })
        .collect()
}

/// Message types that accept a text shorthand besides JSON, with the shorthand's description.
fn shorthand(catalog: &Catalog, type_name: &str) -> Option<String> {
    match type_name {
        "bee.v1.RepoRef" => Some("`org/name` or a repo id".into()),
        "bee.v1.ObjectId" => Some("an object id such as `c1_<64 hex>`".into()),
        "bee.v1.ChangeId" => Some("a change id in text form".into()),
        _ => {
            let message = catalog.message(type_name)?;
            match message.fields.as_slice() {
                [only]
                    if only.cardinality == Cardinality::Single
                        && !matches!(only.kind, Kind::Message(_)) =>
                {
                    Some(format!("the value of its only field `{}`", only.json_name))
                }
                _ => None,
            }
        }
    }
}

/// Well-known types that proto3-JSON writes as something other than an object.
fn well_known(type_name: &str) -> Option<&'static str> {
    Some(match type_name {
        "google.protobuf.Timestamp" => "RFC 3339 timestamp, e.g. 2026-01-02T03:04:05Z",
        "google.protobuf.Duration" => "duration in seconds with an `s` suffix, e.g. 1.5s",
        "google.protobuf.FieldMask" => "comma-separated lowerCamelCase field paths",
        "google.protobuf.Struct" => "JSON object",
        "google.protobuf.Value" => "any JSON value",
        "google.protobuf.ListValue" => "JSON array",
        "google.protobuf.Empty" => "JSON object `{}`",
        "google.protobuf.Any" => "JSON object with `@type`",
        "google.protobuf.BoolValue" => "true or false",
        "google.protobuf.StringValue" => "string",
        "google.protobuf.BytesValue" => "bytes (text, base64:<data> or @file)",
        "google.protobuf.Int32Value" | "google.protobuf.UInt32Value" => "integer",
        "google.protobuf.Int64Value" | "google.protobuf.UInt64Value" => "64-bit integer",
        "google.protobuf.FloatValue" | "google.protobuf.DoubleValue" => "number",
        _ => return None,
    })
}

/// The value placeholder shown in help, e.g. `UINT64`, `ENUM`, `KEY=VALUE`.
pub fn value_name(catalog: &Catalog, field: &Field) -> String {
    if let Cardinality::Map(..) = field.cardinality {
        return "KEY=VALUE".into();
    }
    match &field.kind {
        Kind::Scalar(Scalar::Bool) => "BOOL".into(),
        Kind::Scalar(s) => s.name().to_ascii_uppercase(),
        Kind::Enum(_) => "ENUM".into(),
        Kind::Message(t) if t == "bee.v1.RepoRef" => "ORG/NAME|ID|JSON".into(),
        Kind::Message(t) if t == "google.protobuf.Timestamp" => "TIMESTAMP".into(),
        Kind::Message(t) if t == "google.protobuf.Duration" => "DURATION".into(),
        Kind::Message(t) if well_known(t).is_some() => "VALUE".into(),
        Kind::Message(t) if shorthand(catalog, t).is_some() => "VALUE|JSON".into(),
        Kind::Message(_) => "JSON".into(),
    }
}

/// Description of a value type, e.g. `enum bee.v1.LandMethod: rebase|squash|merge-commit`.
pub fn type_description(catalog: &Catalog, kind: &Kind) -> String {
    match kind {
        Kind::Scalar(Scalar::Bytes) => "bytes: UTF-8 text, base64:<data> or @<file>".into(),
        Kind::Scalar(Scalar::Bool) => "bool: --flag or --flag=false".into(),
        Kind::Scalar(s) => s.name().into(),
        Kind::Enum(name) => match catalog.enum_type(name) {
            Some(e) => format!("enum {name}: {}", e.choices().join("|")),
            None => format!("enum {name}"),
        },
        Kind::Message(name) => match well_known(name) {
            Some(text) => format!("{name}: {text}"),
            None => match shorthand(catalog, name) {
                Some(short) => format!("message {name}: JSON object or {short}"),
                None => format!("message {name}: JSON object"),
            },
        },
    }
}

/// A field's proto comment as one line, for its flag's help.
pub fn flag_description(field: &Field) -> String {
    field
        .comment
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The bracketed facts appended to a flag's help: type, required-ness, repetition, oneof.
pub fn flag_facts(catalog: &Catalog, field: &Field) -> String {
    let mut facts = vec![match &field.cardinality {
        Cardinality::Map(key, value) => format!(
            "map<{}, {}>: KEY=VALUE",
            key.name(),
            type_description(catalog, value)
        ),
        _ => type_description(catalog, &field.kind),
    }];
    facts.push("optional".into());
    if field.cardinality != Cardinality::Single {
        facts.push("repeatable".into());
    }
    if let Some(oneof) = &field.oneof {
        facts.push(format!("oneof `{oneof}`: set at most one"));
    }
    format!("[{}]", facts.join("; "))
}

fn bad(flag: &str, message: impl std::fmt::Display) -> CliError {
    CliError::usage("bad_flag", format!("--{flag}: {message}"))
}

fn parse_bool(flag: &str, raw: &str) -> Result<bool, CliError> {
    match raw.to_ascii_lowercase().as_str() {
        "" | "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(bad(flag, format!("expected true or false, got {raw:?}"))),
    }
}

fn bytes_value(flag: &str, raw: &str) -> Result<Value, CliError> {
    let bytes = if let Some(data) = raw.strip_prefix("base64:") {
        STANDARD
            .decode(data.trim())
            .map_err(|e| bad(flag, format!("invalid base64: {e}")))?
    } else if let Some(path) = raw.strip_prefix('@') {
        std::fs::read(path).map_err(|e| bad(flag, format!("cannot read {path}: {e}")))?
    } else {
        raw.strip_prefix("text:").unwrap_or(raw).as_bytes().to_vec()
    };
    Ok(json!(STANDARD.encode(bytes)))
}

/// Converts one scalar flag value to proto3-JSON.
fn scalar_value(flag: &str, scalar: Scalar, raw: &str) -> Result<Value, CliError> {
    let int = |min: i128, max: i128| -> Result<i128, CliError> {
        let n: i128 = raw
            .trim()
            .parse()
            .map_err(|_| bad(flag, format!("expected an integer, got {raw:?}")))?;
        if n < min || n > max {
            return Err(bad(flag, format!("{n} is out of range {min}..={max}")));
        }
        Ok(n)
    };
    Ok(match scalar {
        Scalar::String => json!(raw),
        Scalar::Bytes => bytes_value(flag, raw)?,
        Scalar::Bool => json!(parse_bool(flag, raw)?),
        Scalar::Int32 => json!(int(i32::MIN.into(), i32::MAX.into())? as i64),
        Scalar::Uint32 => json!(int(0, u32::MAX.into())? as i64),
        Scalar::Int64 => json!(int(i64::MIN.into(), i64::MAX.into())?.to_string()),
        Scalar::Uint64 => json!(int(0, u64::MAX.into())?.to_string()),
        Scalar::Double | Scalar::Float => {
            let x: f64 = raw
                .trim()
                .parse()
                .map_err(|_| bad(flag, format!("expected a number, got {raw:?}")))?;
            if x.is_finite() {
                json!(x)
            } else {
                json!(raw.trim())
            }
        }
    })
}

fn parse_json(flag: &str, raw: &str) -> Result<Value, CliError> {
    serde_json::from_str(raw).map_err(|e| bad(flag, format!("invalid JSON: {e}")))
}

fn id_bytes(flag: &str, type_name: &str, raw: &str) -> Result<Vec<u8>, CliError> {
    let parsed = if type_name == "bee.v1.ObjectId" {
        bee_core::id::ObjectId::parse(raw.trim()).map(|id| id.as_bytes().to_vec())
    } else {
        bee_core::change_id::ChangeId::parse(raw.trim()).map(|id| id.as_bytes().to_vec())
    };
    parsed.map_err(|e| bad(flag, e.to_string()))
}

/// Converts one flag value of a message-typed field.
fn message_value(
    catalog: &Catalog,
    flag: &str,
    type_name: &str,
    raw: &str,
) -> Result<Value, CliError> {
    let trimmed = raw.trim_start();
    match type_name {
        "google.protobuf.Timestamp"
        | "google.protobuf.Duration"
        | "google.protobuf.FieldMask"
        | "google.protobuf.StringValue" => return Ok(json!(raw)),
        "google.protobuf.BytesValue" => return bytes_value(flag, raw),
        "google.protobuf.BoolValue" => return Ok(json!(parse_bool(flag, raw)?)),
        "google.protobuf.Int32Value" => return scalar_value(flag, Scalar::Int32, raw),
        "google.protobuf.UInt32Value" => return scalar_value(flag, Scalar::Uint32, raw),
        "google.protobuf.Int64Value" => return scalar_value(flag, Scalar::Int64, raw),
        "google.protobuf.UInt64Value" => return scalar_value(flag, Scalar::Uint64, raw),
        "google.protobuf.FloatValue" | "google.protobuf.DoubleValue" => {
            return scalar_value(flag, Scalar::Double, raw);
        }
        "google.protobuf.Value" => {
            return Ok(serde_json::from_str(raw).unwrap_or_else(|_| json!(raw)));
        }
        _ => {}
    }
    if trimmed.starts_with('{') || well_known(type_name).is_some() {
        let value = parse_json(flag, raw)?;
        if well_known(type_name).is_none() && !value.is_object() {
            return Err(bad(flag, "expected a JSON object"));
        }
        return Ok(value);
    }
    match type_name {
        "bee.v1.RepoRef" => {
            let key = if raw.contains('/') { "fullName" } else { "id" };
            Ok(json!({ key: raw.trim() }))
        }
        "bee.v1.ObjectId" | "bee.v1.ChangeId" => Ok(json!({
            "raw": STANDARD.encode(id_bytes(flag, type_name, raw)?)
        })),
        _ => {
            let message = catalog
                .message(type_name)
                .ok_or_else(|| bad(flag, format!("unknown message type {type_name}")))?;
            match message.fields.as_slice() {
                [only]
                    if only.cardinality == Cardinality::Single
                        && !matches!(only.kind, Kind::Message(_)) =>
                {
                    let value = single_value(catalog, flag, &only.kind, raw)?;
                    Ok(json!({ only.json_name.clone(): value }))
                }
                _ => Err(bad(
                    flag,
                    format!("expected a JSON object for {type_name}, e.g. '{{\"...\": ...}}'"),
                )),
            }
        }
    }
}

/// Converts one flag value of any kind.
fn single_value(catalog: &Catalog, flag: &str, kind: &Kind, raw: &str) -> Result<Value, CliError> {
    match kind {
        Kind::Scalar(s) => scalar_value(flag, *s, raw),
        Kind::Enum(name) => {
            let e = catalog
                .enum_type(name)
                .ok_or_else(|| bad(flag, format!("unknown enum {name}")))?;
            e.resolve(raw).map(Value::String).ok_or_else(|| {
                bad(
                    flag,
                    format!("{raw:?} is not one of {}", e.choices().join(", ")),
                )
            })
        }
        Kind::Message(name) => message_value(catalog, flag, name, raw),
    }
}

/// The proto3-JSON value of a field from its flag's raw values (one per occurrence).
pub fn field_value(
    catalog: &Catalog,
    flag: &FieldFlag<'_>,
    raw: &[String],
) -> Result<Value, CliError> {
    let field = flag.field;
    let long = flag.long.as_str();
    match &field.cardinality {
        Cardinality::Single => {
            let last = raw.last().map(String::as_str).unwrap_or_default();
            single_value(catalog, long, &field.kind, last)
        }
        Cardinality::Repeated => {
            let mut items = Vec::new();
            for value in raw {
                let is_message = matches!(field.kind, Kind::Message(_));
                if is_message && value.trim_start().starts_with('[') {
                    match parse_json(long, value)? {
                        Value::Array(list) => items.extend(list),
                        _ => return Err(bad(long, "expected a JSON array")),
                    }
                } else {
                    items.push(single_value(catalog, long, &field.kind, value)?);
                }
            }
            Ok(Value::Array(items))
        }
        Cardinality::Map(key_kind, value_kind) => {
            let mut map = Map::new();
            for entry in raw {
                let (key, value) = entry
                    .split_once('=')
                    .ok_or_else(|| bad(long, format!("expected KEY=VALUE, got {entry:?}")))?;
                if *key_kind != Scalar::String {
                    scalar_value(long, *key_kind, key)?;
                }
                map.insert(
                    key.to_owned(),
                    single_value(catalog, long, value_kind, value)?,
                );
            }
            Ok(Value::Object(map))
        }
    }
}

/// Fails with `INVALID_ARGUMENT`/`unknown_field` if `value` names a field that the message (or
/// a nested message) does not have, so typos in JSON input are not silently ignored.
pub fn check_known_fields(
    catalog: &Catalog,
    type_name: &str,
    value: &Value,
    at: &str,
) -> Result<(), CliError> {
    let (Some(message), Some(object)) = (catalog.message(type_name), value.as_object()) else {
        return Ok(());
    };
    for (key, child) in object {
        let Some(field) = message.field(key) else {
            let known: Vec<&str> = message
                .fields
                .iter()
                .map(|f| f.json_name.as_str())
                .collect();
            return Err(CliError::usage(
                "unknown_field",
                format!(
                    "{at}{key}: {type_name} has no field {key:?} (fields: {})",
                    known.join(", ")
                ),
            ));
        };
        let Kind::Message(child_type) = &field.kind else {
            continue;
        };
        let path = format!("{at}{key}.");
        match (&field.cardinality, child) {
            (Cardinality::Single, _) => check_known_fields(catalog, child_type, child, &path)?,
            (Cardinality::Repeated, Value::Array(items)) => {
                for item in items {
                    check_known_fields(catalog, child_type, item, &path)?;
                }
            }
            (Cardinality::Map(_, Kind::Message(value_type)), Value::Object(entries)) => {
                for item in entries.values() {
                    check_known_fields(catalog, value_type, item, &path)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Sets `field` in a request object, replacing any value given under its JSON or proto name.
pub fn set_field(request: &mut Map<String, Value>, field: &Field, value: Value) {
    request.remove(&field.name);
    request.insert(field.json_name.clone(), value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::catalog;

    fn flag_for<'a>(message: &'a MessageType, name: &str) -> FieldFlag<'a> {
        field_flags(message, false)
            .into_iter()
            .find(|f| f.field.name == name)
            .expect("field")
    }

    #[test]
    fn scalars_enums_and_shorthands() {
        let catalog = catalog();
        let land = catalog.message("bee.v1.LandRequest").expect("land");
        let raw = |v: &str| vec![v.to_owned()];
        assert_eq!(
            field_value(catalog, &flag_for(land, "repo"), &raw("acme/app")).expect("repo"),
            json!({"fullName": "acme/app"})
        );
        assert_eq!(
            field_value(catalog, &flag_for(land, "repo"), &raw("01HZX")).expect("repo id"),
            json!({"id": "01HZX"})
        );
        assert_eq!(
            field_value(catalog, &flag_for(land, "expected_version"), &raw("42")).expect("u64"),
            json!("42")
        );
        assert!(field_value(catalog, &flag_for(land, "expected_version"), &raw("-1")).is_err());
        assert_eq!(
            field_value(catalog, &flag_for(land, "method"), &raw("squash")).expect("enum"),
            json!("LAND_METHOD_SQUASH")
        );
        assert_eq!(
            field_value(catalog, &flag_for(land, "allow_empty"), &raw("")).expect("bool"),
            json!(true)
        );
        let id = format!("c1_{}", "ab".repeat(32));
        let head = field_value(catalog, &flag_for(land, "head"), &raw(&id)).expect("id");
        assert!(head["raw"].is_string());
        assert!(field_value(catalog, &flag_for(land, "head"), &raw("nope")).is_err());
    }

    #[test]
    fn bytes_forms() {
        assert_eq!(bytes_value("x", "src/a").expect("text"), json!("c3JjL2E="));
        assert_eq!(
            bytes_value("x", "base64:c3JjL2E=").expect("b64"),
            json!("c3JjL2E=")
        );
        assert_eq!(bytes_value("x", "text:@a").expect("literal"), json!("QGE="));
        assert!(bytes_value("x", "base64:!!").is_err());
    }

    #[test]
    fn reserved_names_get_a_suffix() {
        let catalog = catalog();
        for (_, verb) in catalog.commands() {
            let message = catalog.message(verb.method.input_type).expect("request");
            let follow_field = crate::cli::follow_is_field(catalog, verb);
            for flag in field_flags(message, follow_field) {
                let plain_follow = follow_field && flag.long == "follow";
                assert!(
                    plain_follow || !RESERVED.contains(&flag.long.as_str()),
                    "{} --{}",
                    verb.method.path,
                    flag.long
                );
            }
        }
        let synthetic = MessageType {
            name: "x.Y".into(),
            comment: String::new(),
            fields: vec![Field {
                name: "token".into(),
                json_name: "token".into(),
                kind: Kind::Scalar(Scalar::String),
                cardinality: Cardinality::Single,
                oneof: None,
                comment: String::new(),
            }],
        };
        assert_eq!(field_flags(&synthetic, false)[0].long, "token-field");
        let tail = catalog
            .noun("workflow")
            .and_then(|n| n.verb("tail-workflow-job-logs"))
            .expect("tail");
        assert!(crate::cli::follow_is_field(catalog, tail));
    }
}
