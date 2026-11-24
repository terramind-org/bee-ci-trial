//! A small JSON Schema (draft 2020-12) validator covering the keywords
//! `schemas/v1/import-report.schema.json` uses: `type` (single or list), `const`, `enum`,
//! `required`, `properties`, `additionalProperties: false`, `items`, `minimum` and local
//! `$ref`s into `$defs`. Unknown keywords are refused so the schema cannot silently outgrow
//! what this checks.

use serde_json::Value;

const KNOWN: &[&str] = &[
    "$schema",
    "$id",
    "$defs",
    "$ref",
    "title",
    "description",
    "type",
    "const",
    "enum",
    "required",
    "properties",
    "additionalProperties",
    "items",
    "minimum",
];

fn type_ok(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        other => panic!("schema uses unknown type {other}"),
    }
}

/// Every violation of `schema` by `value`, as `path: message`.
pub fn validate(root: &Value, value: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check(root, root, value, "$", &mut errors);
    errors
}

fn check(root: &Value, schema: &Value, v: &Value, path: &str, errors: &mut Vec<String>) {
    let s = schema.as_object().expect("schema nodes are objects");
    for k in s.keys() {
        assert!(
            KNOWN.contains(&k.as_str()),
            "validator does not know keyword {k}"
        );
    }
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let name = r.strip_prefix("#/$defs/").expect("only local $defs refs");
        check(root, &root["$defs"][name], v, path, errors);
    }
    if let Some(t) = s.get("type") {
        let ok = match t {
            Value::String(t) => type_ok(t, v),
            Value::Array(ts) => ts.iter().any(|t| type_ok(t.as_str().unwrap(), v)),
            _ => panic!("bad type keyword"),
        };
        if !ok {
            errors.push(format!("{path}: expected type {t}, got {v}"));
            return;
        }
    }
    if let Some(c) = s.get("const")
        && c != v
    {
        errors.push(format!("{path}: expected {c}"));
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        errors.push(format!("{path}: {v} not in enum"));
    }
    if let Some(m) = s.get("minimum").and_then(Value::as_i64)
        && v.as_i64().is_some_and(|n| n < m)
    {
        errors.push(format!("{path}: below minimum {m}"));
    }
    if let Some(obj) = v.as_object() {
        if let Some(req) = s.get("required").and_then(Value::as_array) {
            for r in req {
                let r = r.as_str().unwrap();
                if !obj.contains_key(r) {
                    errors.push(format!("{path}: missing required {r}"));
                }
            }
        }
        let props = s.get("properties").and_then(Value::as_object);
        for (k, val) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(ps) => check(root, ps, val, &format!("{path}.{k}"), errors),
                None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    errors.push(format!("{path}: unexpected property {k}"))
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(arr)) = (s.get("items"), v.as_array()) {
        for (i, item) in arr.iter().enumerate() {
            check(root, items, item, &format!("{path}[{i}]"), errors);
        }
    }
}
