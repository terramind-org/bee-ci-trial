//! Output: the `--json` envelope `{"schema": "bee.v1.<command>", "data": ...}` and the
//! readable rendering used without `--json`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::catalog::{Cardinality, Catalog, Kind};

/// The success document of a command.
pub fn envelope(schema: &str, data: Value) -> Value {
    json!({"schema": schema, "data": data})
}

/// The schema id of a command path: `["write", "land"]` -> `bee.v1.write.land`.
pub fn schema_id(path: &[&str]) -> String {
    format!("bee.v1.{}", path.join("."))
}

/// Renders a proto3-JSON message readably, using the descriptor to show ids in text form.
pub fn render_message(catalog: &Catalog, type_name: &str, value: &Value) -> String {
    let mut out = String::new();
    write_value(catalog, Some(type_name), value, 0, &mut out);
    let text = out.trim_end().to_owned();
    if text.is_empty() || text == "{}" {
        "ok".to_owned()
    } else {
        text
    }
}

/// Renders any JSON value readably.
pub fn render_value(value: &Value) -> String {
    let mut out = String::new();
    write_value(crate::catalog::catalog(), None, value, 0, &mut out);
    let text = out.trim_end().to_owned();
    if text.is_empty() {
        "ok".to_owned()
    } else {
        text
    }
}

/// The text form of an `ObjectId` or `ChangeId` value (`{"raw": base64}`), if it decodes.
fn id_text(type_name: &str, value: &Value) -> Option<String> {
    let raw = value.get("raw")?.as_str()?;
    let bytes = STANDARD.decode(raw).ok()?;
    match type_name {
        "bee.v1.ObjectId" => bee_core::id::ObjectId::from_bytes(&bytes)
            .ok()
            .map(|id| id.to_text()),
        "bee.v1.ChangeId" => bee_core::change_id::ChangeId::from_bytes(&bytes)
            .ok()
            .map(|id| id.to_text()),
        _ => None,
    }
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

fn inline_text(type_name: Option<&str>, value: &Value) -> Option<String> {
    if let Some(name) = type_name
        && let Some(text) = id_text(name, value)
    {
        return Some(text);
    }
    match value {
        Value::Object(map) if map.is_empty() => Some("{}".into()),
        Value::Array(items) if items.is_empty() => Some("[]".into()),
        Value::Array(items)
            if items.len() <= 8
                && items
                    .iter()
                    .all(|i| !i.is_object() && !i.is_array() && scalar_text(i).len() <= 40) =>
        {
            Some(format!(
                "[{}]",
                items.iter().map(scalar_text).collect::<Vec<_>>().join(", ")
            ))
        }
        Value::Object(_) | Value::Array(_) => None,
        other => Some(scalar_text(other)),
    }
}

/// The message type of the elements a field holds, if they are messages.
fn child_type(catalog: &Catalog, parent: Option<&str>, key: &str) -> Option<String> {
    let field = catalog.message(parent?)?.field(key)?;
    match (&field.cardinality, &field.kind) {
        (Cardinality::Map(_, Kind::Message(t)), _) => Some(format!("map:{t}")),
        (Cardinality::Map(..), _) => None,
        (_, Kind::Message(t)) => Some(t.clone()),
        _ => None,
    }
}

fn write_value(
    catalog: &Catalog,
    type_name: Option<&str>,
    value: &Value,
    indent: usize,
    out: &mut String,
) {
    let pad = "  ".repeat(indent);
    match value {
        Value::Object(map) => {
            if let Some(text) = type_name.and_then(|t| id_text(t, value)) {
                out.push_str(&format!("{pad}{text}\n"));
                return;
            }
            let map_value_type = type_name.and_then(|t| t.strip_prefix("map:"));
            for (key, child) in map {
                let child_type = match map_value_type {
                    Some(t) => Some(t.to_owned()),
                    None => child_type(catalog, type_name, key),
                };
                match inline_text(child_type.as_deref(), child) {
                    Some(text) => out.push_str(&format!("{pad}{key}: {text}\n")),
                    None => {
                        out.push_str(&format!("{pad}{key}:\n"));
                        write_value(catalog, child_type.as_deref(), child, indent + 1, out);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                match inline_text(type_name, item) {
                    Some(text) => out.push_str(&format!("{pad}- {text}\n")),
                    None => {
                        out.push_str(&format!("{pad}-\n"));
                        write_value(catalog, type_name, item, indent + 1, out);
                    }
                }
            }
        }
        other => out.push_str(&format!("{pad}{}\n", scalar_text(other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::catalog;

    #[test]
    fn renders_ids_as_text() {
        let id = bee_core::id::ObjectId::new(bee_core::id::ObjectKind::Changeset, [7; 32]);
        let value = json!({
            "landed": [{"result": {"raw": STANDARD.encode(id.as_bytes())}, "skipped": true}],
            "eventSeq": "3",
        });
        let text = render_message(catalog(), "bee.v1.LandResponse", &value);
        assert!(
            text.contains(&format!("result: {}", id.to_text())),
            "{text}"
        );
        assert!(text.contains("eventSeq: 3"), "{text}");
        assert_eq!(
            render_message(catalog(), "bee.v1.LandResponse", &json!({})),
            "ok"
        );
    }

    #[test]
    fn renders_nested_values() {
        let text = render_value(&json!({"a": {"b": [1, 2]}, "c": [{"d": "e"}]}));
        assert_eq!(text, "a:\n  b: [1, 2]\nc:\n  -\n    d: e");
        assert_eq!(schema_id(&["write", "land"]), "bee.v1.write.land");
    }
}
