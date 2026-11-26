//! The schema subset the gateway serves (`schema.graphql`, a hand-written slice of GitHub's
//! public schema covering what `gh` and common integrations query), its type relations and
//! introspection (`__schema`, `__type`), which GHES-mode `gh` uses for feature detection.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{Value, json};

use super::parse::{FieldDef, Kind, TypeDef, TypeRef, parse_schema};

const SDL: &str = include_str!("schema.graphql");

const INTROSPECTION: &str = r#"
enum __TypeKind { SCALAR OBJECT INTERFACE UNION ENUM INPUT_OBJECT LIST NON_NULL }
type __Schema { description: String types: [__Type!]! queryType: __Type! mutationType: __Type subscriptionType: __Type directives: [__Directive!]! }
type __Type { kind: __TypeKind! name: String description: String fields(includeDeprecated: Boolean): [__Field!] interfaces: [__Type!] possibleTypes: [__Type!] enumValues(includeDeprecated: Boolean): [__EnumValue!] inputFields(includeDeprecated: Boolean): [__InputValue!] ofType: __Type specifiedByURL: String }
type __Field { name: String! description: String args(includeDeprecated: Boolean): [__InputValue!]! type: __Type! isDeprecated: Boolean! deprecationReason: String }
type __InputValue { name: String! description: String type: __Type! defaultValue: String isDeprecated: Boolean! deprecationReason: String }
type __EnumValue { name: String! description: String isDeprecated: Boolean! deprecationReason: String }
type __Directive { name: String! description: String locations: [String!]! args: [__InputValue!]! isRepeatable: Boolean! }
"#;

/// The parsed schema.
pub(crate) struct Schema {
    types: BTreeMap<String, TypeDef>,
}

/// The schema, parsed once.
pub(crate) fn schema() -> &'static Schema {
    static S: OnceLock<Schema> = OnceLock::new();
    S.get_or_init(|| {
        let mut types = BTreeMap::new();
        for src in [SDL, INTROSPECTION] {
            for t in parse_schema(src).unwrap_or_default() {
                types.insert(t.name.clone(), t);
            }
        }
        for s in ["String", "Int", "Float", "Boolean", "ID"] {
            types.insert(
                s.to_string(),
                TypeDef {
                    name: s.to_string(),
                    kind: Kind::Scalar,
                    fields: Vec::new(),
                    interfaces: Vec::new(),
                    members: Vec::new(),
                    values: Vec::new(),
                },
            );
        }
        Schema { types }
    })
}

impl Schema {
    /// A type.
    pub fn get(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(name)
    }

    /// A field of a type (interfaces included).
    pub fn field(&self, ty: &str, name: &str) -> Option<&FieldDef> {
        self.types.get(ty)?.fields.iter().find(|f| f.name == name)
    }

    /// Whether an object of type `obj` satisfies the type condition `cond`.
    pub fn matches(&self, obj: &str, cond: &str) -> bool {
        if obj == cond {
            return true;
        }
        match self.types.get(cond) {
            Some(t) if t.kind == Kind::Union => t.members.iter().any(|m| m == obj),
            Some(t) if t.kind == Kind::Interface => self
                .types
                .get(obj)
                .is_some_and(|o| o.interfaces.iter().any(|i| i == cond)),
            _ => false,
        }
    }

    fn kind_word(k: Kind) -> &'static str {
        match k {
            Kind::Scalar => "SCALAR",
            Kind::Object => "OBJECT",
            Kind::Interface => "INTERFACE",
            Kind::Union => "UNION",
            Kind::Enum => "ENUM",
            Kind::Input => "INPUT_OBJECT",
        }
    }

    /// Introspection for a type reference.
    fn type_ref_json(&self, t: &TypeRef) -> Value {
        match t {
            TypeRef::NonNull(inner) => {
                json!({"kind": "NON_NULL", "name": Value::Null, "ofType": self.type_ref_json(inner)})
            }
            TypeRef::List(inner) => {
                json!({"kind": "LIST", "name": Value::Null, "ofType": self.type_ref_json(inner)})
            }
            TypeRef::Named(n) => {
                let kind = self
                    .types
                    .get(n)
                    .map(|t| Self::kind_word(t.kind))
                    .unwrap_or("SCALAR");
                json!({"kind": kind, "name": n, "ofType": Value::Null})
            }
        }
    }

    fn input_json(&self, name: &str, ty: &TypeRef, default: Option<String>) -> Value {
        json!({
            "name": name, "description": Value::Null, "type": self.type_ref_json(ty),
            "defaultValue": default, "isDeprecated": false, "deprecationReason": Value::Null,
        })
    }

    /// Introspection for a named type (`__type(name:)`).
    pub fn type_json(&self, name: &str) -> Value {
        let Some(t) = self.types.get(name) else {
            return Value::Null;
        };
        let named = |n: &String| self.type_ref_json(&TypeRef::Named(n.clone()));
        let fields = match t.kind {
            Kind::Object | Kind::Interface => Value::Array(
                t.fields
                    .iter()
                    .map(|f| {
                        json!({
                            "name": f.name,
                            "description": Value::Null,
                            "args": f.args.iter().map(|(n, ty, d)| self.input_json(n, ty, d.as_ref().map(|v| format!("{v:?}")))).collect::<Vec<_>>(),
                            "type": self.type_ref_json(&f.ty),
                            "isDeprecated": false,
                            "deprecationReason": Value::Null,
                        })
                    })
                    .collect(),
            ),
            _ => Value::Null,
        };
        let possible: Vec<Value> = match t.kind {
            Kind::Union => t.members.iter().map(named).collect(),
            Kind::Interface => self
                .types
                .values()
                .filter(|o| o.interfaces.iter().any(|i| i == name))
                .map(|o| named(&o.name))
                .collect(),
            _ => Vec::new(),
        };
        json!({
            "kind": Self::kind_word(t.kind),
            "name": t.name,
            "description": Value::Null,
            "fields": fields,
            "interfaces": if t.kind == Kind::Object || t.kind == Kind::Interface { Value::Array(t.interfaces.iter().map(named).collect()) } else { Value::Null },
            "possibleTypes": if possible.is_empty() { Value::Null } else { Value::Array(possible) },
            "enumValues": if t.kind == Kind::Enum { Value::Array(t.values.iter().map(|v| json!({"name": v, "description": Value::Null, "isDeprecated": false, "deprecationReason": Value::Null})).collect()) } else { Value::Null },
            "inputFields": if t.kind == Kind::Input { Value::Array(t.fields.iter().map(|f| self.input_json(&f.name, &f.ty, None)).collect()) } else { Value::Null },
            "ofType": Value::Null,
            "specifiedByURL": Value::Null,
        })
    }

    /// Introspection for the whole schema (`__schema`).
    pub fn schema_json(&self) -> Value {
        json!({
            "description": Value::Null,
            "types": self.types.keys().map(|n| self.type_json(n)).collect::<Vec<_>>(),
            "queryType": self.type_json("Query"),
            "mutationType": self.type_json("Mutation"),
            "subscriptionType": Value::Null,
            "directives": [
                {"name": "include", "description": Value::Null, "locations": ["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"], "isRepeatable": false,
                 "args": [self.input_json("if", &TypeRef::NonNull(Box::new(TypeRef::Named("Boolean".into()))), None)]},
                {"name": "skip", "description": Value::Null, "locations": ["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"], "isRepeatable": false,
                 "args": [self.input_json("if", &TypeRef::NonNull(Box::new(TypeRef::Named("Boolean".into()))), None)]},
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_parses_and_relates_types() {
        let s = schema();
        assert!(s.get("PullRequest").is_some(), "schema.graphql must parse");
        assert!(s.field("Repository", "pullRequests").is_some());
        assert!(s.matches("CheckRun", "StatusCheckRollupContext"));
        assert!(s.matches("PullRequest", "Node"));
        assert!(s.matches("User", "Actor"));
        assert!(!s.matches("Issue", "PullRequest"));
        let t = s.type_json("PullRequest");
        let names: Vec<&str> = t["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"statusCheckRollup") && names.contains(&"headRefName"));
        assert_eq!(s.type_json("SearchType")["kind"], "ENUM");
        assert_eq!(s.type_json("Nope"), Value::Null);
    }

    #[test]
    fn every_field_type_is_defined() {
        let s = schema();
        for t in s.types.values() {
            for f in &t.fields {
                assert!(
                    s.get(f.ty.base()).is_some(),
                    "{}.{}: {}",
                    t.name,
                    f.name,
                    f.ty.base()
                );
                for (a, ty, _) in &f.args {
                    assert!(
                        s.get(ty.base()).is_some(),
                        "{}.{}({a}): {}",
                        t.name,
                        f.name,
                        ty.base()
                    );
                }
            }
            for i in &t.interfaces {
                let iface = s
                    .get(i)
                    .unwrap_or_else(|| panic!("{} implements unknown {i}", t.name));
                for f in &iface.fields {
                    assert!(
                        t.fields.iter().any(|x| x.name == f.name),
                        "{} lacks {}.{}",
                        t.name,
                        i,
                        f.name
                    );
                }
            }
        }
    }
}
