//! Request templates: strings in fixtures refer to variables saved by the scenario and by
//! setup steps.
//!
//! - A string that is exactly `{{name.path}}` becomes the variable's JSON value (any type).
//! - `{{...}}` inside a longer string is replaced by the value's text (strings as is, other
//!   values as compact JSON).
//! - A path walks objects by key and arrays by index: `{{cr.change.number}}`,
//!   `{{issues.issues.0.id}}`.
//!
//! An unknown variable or path is an error, so a fixture never silently sends an empty value.

use serde_json::{Map, Value};

/// Named JSON values.
#[derive(Clone, Debug, Default)]
pub struct Vars {
    values: Map<String, Value>,
}

impl Vars {
    /// No variables.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `name`.
    pub fn set(&mut self, name: &str, value: Value) {
        self.values.insert(name.to_owned(), value);
    }

    /// The value at `path` (`name` or `name.key.0.key`; `\.` is a dot inside a key, as in
    /// `c.blobs.README\.md.id`).
    pub fn lookup(&self, path: &str) -> Option<&Value> {
        let mut parts: Vec<String> = vec![String::new()];
        let mut chars = path.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => parts.last_mut()?.push(chars.next()?),
                '.' => parts.push(String::new()),
                other => parts.last_mut()?.push(other),
            }
        }
        let mut parts = parts.iter();
        let mut current = self.values.get(parts.next()?.as_str())?;
        for part in parts {
            let part = part.as_str();
            current = match current {
                Value::Object(map) => map.get(part)?,
                Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(current)
    }

    /// Every variable, by name.
    pub fn values(&self) -> &Map<String, Value> {
        &self.values
    }
}

/// Expands every template string in `value`.
pub fn expand(value: &Value, vars: &Vars) -> Result<Value, String> {
    Ok(match value {
        Value::String(s) => expand_string(s, vars)?,
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| expand(v, vars))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), expand(v, vars)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

fn expand_string(s: &str, vars: &Vars) -> Result<Value, String> {
    let trimmed = s.trim();
    if let Some(inner) = trimmed
        .strip_prefix("{{")
        .and_then(|r| r.strip_suffix("}}"))
        && !inner.contains("{{")
        && !inner.contains("}}")
    {
        return vars
            .lookup(inner.trim())
            .cloned()
            .ok_or_else(|| format!("unknown template variable {{{{{}}}}}", inner.trim()));
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| format!("unclosed template in {s:?}"))?;
        let name = after[..end].trim();
        let value = vars
            .lookup(name)
            .ok_or_else(|| format!("unknown template variable {{{{{name}}}}}"))?;
        match value {
            Value::String(text) => out.push_str(text),
            other => out.push_str(&other.to_string()),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(Value::String(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whole_and_embedded() {
        let mut vars = Vars::new();
        vars.set("repo", json!("dev/playground"));
        vars.set("cr", json!({"change": {"number": "7", "ids": [1, 2]}}));
        let out = expand(
            &json!({"repo": {"fullName": "{{repo}}"}, "n": "{{cr.change.number}}",
                    "x": "{{ cr.change.ids }}", "y": "#{{cr.change.ids.1}} in {{repo}}",
                    "keep": [true, 3]}),
            &vars,
        )
        .expect("expands");
        assert_eq!(
            out,
            json!({"repo": {"fullName": "dev/playground"}, "n": "7", "x": [1, 2],
                   "y": "#2 in dev/playground", "keep": [true, 3]})
        );
        vars.set("c", json!({"blobs": {"README.md": {"id": "b1"}}}));
        assert_eq!(
            expand(&json!("{{c.blobs.README\\.md.id}}"), &vars).expect("escaped dot"),
            json!("b1")
        );
        assert!(expand(&json!("{{nope}}"), &vars).is_err());
        assert!(expand(&json!("a {{cr.change.zzz}}"), &vars).is_err());
        assert!(expand(&json!("a {{repo"), &vars).is_err());
    }
}
