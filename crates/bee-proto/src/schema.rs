//! OpenAPI 3.1 and JSON Schema documents for the HTTP/JSON surface, generated from the
//! descriptors so they can never drift from the protos.
//!
//! - `schemas/openapi/bee.v1.json` describes every RPC as `POST /bee.v1.<Service>/<Method>`
//!   with proto3-JSON bodies and the Connect error shape.
//! - `schemas/v1/messages/<message>.schema.json` holds one self-contained JSON Schema (draft
//!   2020-12) per RPC request and response message; MCP tools use them as input and output
//!   schemas.
//!
//! The JSON mapping is proto3-JSON: lowerCamelCase names, 64-bit integers as decimal strings,
//! `bytes` as base64, enums as value names, `Timestamp` as RFC 3339. Parsers also accept the
//! original field names, numbers for 64-bit integers and enum numbers, and ignore unknown fields.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    FileDescriptorSet, SourceCodeInfo,
};
use serde_json::{Map, Value, json};

use crate::methods::{METHODS, MethodKind};

/// Path of the OpenAPI document, relative to the repository root.
pub const OPENAPI_PATH: &str = "schemas/openapi/bee.v1.json";

/// Directory of the per-message JSON Schemas, relative to the repository root.
pub const MESSAGES_DIR: &str = "schemas/v1/messages";

/// Base URI of the published schemas, used for `$id`.
pub const SCHEMA_BASE_URI: &str = "https://bee.dev/";

/// One message with what the generators need.
#[derive(Clone, Debug)]
struct MessageInfo {
    descriptor: DescriptorProto,
    comment: String,
    field_comments: Vec<String>,
    /// A synthetic `map<K, V>` entry message.
    is_map_entry: bool,
}

/// One enum with its comments.
#[derive(Clone, Debug)]
struct EnumInfo {
    descriptor: EnumDescriptorProto,
    comment: String,
}

/// One service with its comments.
#[derive(Clone, Debug)]
struct ServiceInfo {
    name: String,
    comment: String,
    method_comments: BTreeMap<String, String>,
}

/// Every type and service in the descriptor set, keyed by fully qualified name without the
/// leading dot.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    messages: BTreeMap<String, MessageInfo>,
    enums: BTreeMap<String, EnumInfo>,
    services: BTreeMap<String, ServiceInfo>,
}

fn comments_by_path(info: Option<&SourceCodeInfo>) -> BTreeMap<Vec<i32>, String> {
    let mut map = BTreeMap::new();
    for location in info.map(|i| i.location.as_slice()).unwrap_or_default() {
        let raw = location
            .leading_comments
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .or(location.trailing_comments.as_deref())
            .unwrap_or_default();
        let text = raw
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        if !text.is_empty() {
            map.insert(location.path.clone(), text);
        }
    }
    map
}

impl Catalog {
    /// The catalog of [`crate::FILE_DESCRIPTOR_SET`].
    pub fn load() -> Self {
        let set = FileDescriptorSet::decode(crate::FILE_DESCRIPTOR_SET)
            .unwrap_or_else(|e| panic!("embedded descriptor set does not decode: {e}"));
        Self::from_set(&set)
    }

    /// The catalog of a descriptor set.
    pub fn from_set(set: &FileDescriptorSet) -> Self {
        let mut catalog = Self::default();
        for file in &set.file {
            catalog.add_file(file);
        }
        catalog
    }

    fn add_file(&mut self, file: &FileDescriptorProto) {
        let comments = comments_by_path(file.source_code_info.as_ref());
        let package = file.package();
        for (i, message) in file.message_type.iter().enumerate() {
            self.add_message(package, message, vec![4, i as i32], &comments);
        }
        for (i, e) in file.enum_type.iter().enumerate() {
            self.add_enum(package, e, vec![5, i as i32], &comments);
        }
        for (i, service) in file.service.iter().enumerate() {
            let path = vec![6, i as i32];
            let method_comments = service
                .method
                .iter()
                .enumerate()
                .map(|(j, m)| {
                    let mut p = path.clone();
                    p.extend([2, j as i32]);
                    (
                        m.name().to_owned(),
                        comments.get(&p).cloned().unwrap_or_default(),
                    )
                })
                .collect();
            let name = format!("{package}.{}", service.name());
            self.services.insert(
                name.clone(),
                ServiceInfo {
                    name,
                    comment: comments.get(&path).cloned().unwrap_or_default(),
                    method_comments,
                },
            );
        }
    }

    fn add_message(
        &mut self,
        scope: &str,
        message: &DescriptorProto,
        path: Vec<i32>,
        comments: &BTreeMap<Vec<i32>, String>,
    ) {
        let name = format!("{scope}.{}", message.name());
        let field_comments = (0..message.field.len())
            .map(|k| {
                let mut p = path.clone();
                p.extend([2, k as i32]);
                comments.get(&p).cloned().unwrap_or_default()
            })
            .collect();
        for (j, nested) in message.nested_type.iter().enumerate() {
            let mut p = path.clone();
            p.extend([3, j as i32]);
            self.add_message(&name, nested, p, comments);
        }
        for (j, e) in message.enum_type.iter().enumerate() {
            let mut p = path.clone();
            p.extend([4, j as i32]);
            self.add_enum(&name, e, p, comments);
        }
        self.messages.insert(
            name,
            MessageInfo {
                descriptor: message.clone(),
                comment: comments.get(&path).cloned().unwrap_or_default(),
                field_comments,
                is_map_entry: message.options.as_ref().is_some_and(|o| o.map_entry()),
            },
        );
    }

    fn add_enum(
        &mut self,
        scope: &str,
        e: &EnumDescriptorProto,
        path: Vec<i32>,
        comments: &BTreeMap<Vec<i32>, String>,
    ) {
        self.enums.insert(
            format!("{scope}.{}", e.name()),
            EnumInfo {
                descriptor: e.clone(),
                comment: comments.get(&path).cloned().unwrap_or_default(),
            },
        );
    }

    /// Names of every `bee.v1` message that is not a map entry.
    pub fn message_names(&self) -> Vec<&str> {
        self.messages
            .iter()
            .filter(|(name, info)| name.starts_with("bee.") && !info.is_map_entry)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// JSON Schema of a well-known type that proto3-JSON maps to a scalar, if `name` is one.
    fn well_known(name: &str) -> Option<Value> {
        Some(match name {
            "google.protobuf.Timestamp" => json!({"type": "string", "format": "date-time"}),
            "google.protobuf.Duration" => {
                json!({"type": "string", "pattern": "^-?[0-9]+(\\.[0-9]{1,9})?s$"})
            }
            "google.protobuf.Empty" => json!({"type": "object"}),
            "google.protobuf.FieldMask" => json!({"type": "string"}),
            "google.protobuf.Struct" => json!({"type": "object"}),
            "google.protobuf.Value" => json!({}),
            "google.protobuf.ListValue" => json!({"type": "array"}),
            "google.protobuf.Any" => {
                json!({"type": "object", "properties": {"@type": {"type": "string"}}})
            }
            "google.protobuf.BoolValue" => json!({"type": "boolean"}),
            "google.protobuf.StringValue" => json!({"type": "string"}),
            "google.protobuf.BytesValue" => json!({"type": "string", "contentEncoding": "base64"}),
            "google.protobuf.Int32Value" | "google.protobuf.UInt32Value" => {
                json!({"type": "integer"})
            }
            "google.protobuf.Int64Value" | "google.protobuf.UInt64Value" => {
                json!({"type": ["string", "integer"], "pattern": "^-?[0-9]+$"})
            }
            "google.protobuf.FloatValue" | "google.protobuf.DoubleValue" => {
                json!({"type": "number"})
            }
            _ => return None,
        })
    }

    /// Schema of one field value (not counting `repeated`).
    fn value_schema(&self, field: &FieldDescriptorProto, refs: &dyn Fn(&str) -> String) -> Value {
        let type_name = field.type_name().trim_start_matches('.');
        match field.r#type() {
            Type::Double | Type::Float => json!({"type": "number"}),
            Type::Int32 | Type::Sint32 | Type::Sfixed32 => {
                json!({"type": "integer", "format": "int32"})
            }
            Type::Uint32 | Type::Fixed32 => {
                json!({"type": "integer", "format": "uint32", "minimum": 0})
            }
            Type::Int64 | Type::Sint64 | Type::Sfixed64 => {
                json!({"type": ["string", "integer"], "format": "int64", "pattern": "^-?[0-9]+$"})
            }
            Type::Uint64 | Type::Fixed64 => {
                json!({"type": ["string", "integer"], "format": "uint64", "pattern": "^[0-9]+$"})
            }
            Type::Bool => json!({"type": "boolean"}),
            Type::String => json!({"type": "string"}),
            Type::Bytes => json!({"type": "string", "contentEncoding": "base64"}),
            Type::Enum => json!({"$ref": refs(type_name)}),
            Type::Message | Type::Group => {
                Self::well_known(type_name).unwrap_or_else(|| json!({"$ref": refs(type_name)}))
            }
        }
    }

    fn field_schema(&self, field: &FieldDescriptorProto, refs: &dyn Fn(&str) -> String) -> Value {
        let type_name = field.type_name().trim_start_matches('.');
        if field.label() == Label::Repeated {
            if let Some(entry) = self.messages.get(type_name).filter(|m| m.is_map_entry) {
                let value_field = entry
                    .descriptor
                    .field
                    .iter()
                    .find(|f| f.number() == 2)
                    .map(|f| self.value_schema(f, refs))
                    .unwrap_or_else(|| json!({}));
                return json!({"type": "object", "additionalProperties": value_field});
            }
            return json!({"type": "array", "items": self.value_schema(field, refs)});
        }
        self.value_schema(field, refs)
    }

    /// Schema of a message's own shape; references go through `refs`.
    fn message_body(&self, name: &str, refs: &dyn Fn(&str) -> String) -> Option<Value> {
        let info = self.messages.get(name)?;
        let mut properties = Map::new();
        for (field, comment) in info.descriptor.field.iter().zip(&info.field_comments) {
            let mut schema = self.field_schema(field, refs);
            if !comment.is_empty() {
                with_description(&mut schema, comment);
            }
            properties.insert(field.json_name().to_owned(), schema);
        }
        let mut description = info.comment.clone();
        for (index, oneof) in info.descriptor.oneof_decl.iter().enumerate() {
            let members: Vec<String> = info
                .descriptor
                .field
                .iter()
                .filter(|f| f.oneof_index == Some(index as i32) && !f.proto3_optional())
                .map(|f| format!("`{}`", f.json_name()))
                .collect();
            if members.len() > 1 {
                if !description.is_empty() {
                    description.push_str("\n\n");
                }
                description.push_str(&format!(
                    "Oneof `{}`: at most one of {} is set.",
                    oneof.name(),
                    members.join(", ")
                ));
            }
        }
        let mut schema = Map::new();
        schema.insert("type".into(), json!("object"));
        schema.insert("title".into(), json!(name));
        if !description.is_empty() {
            schema.insert("description".into(), json!(description));
        }
        schema.insert("properties".into(), Value::Object(properties));
        Some(Value::Object(schema))
    }

    fn enum_body(&self, name: &str) -> Option<Value> {
        let info = self.enums.get(name)?;
        let values: Vec<&str> = info.descriptor.value.iter().map(|v| v.name()).collect();
        let mut schema = Map::new();
        schema.insert("type".into(), json!("string"));
        schema.insert("title".into(), json!(name));
        if !info.comment.is_empty() {
            schema.insert("description".into(), json!(info.comment));
        }
        schema.insert("enum".into(), json!(values));
        Some(Value::Object(schema))
    }

    /// Message and enum names a message refers to, directly.
    fn direct_refs(&self, name: &str) -> Vec<String> {
        let Some(info) = self.messages.get(name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for field in &info.descriptor.field {
            let type_name = field.type_name().trim_start_matches('.');
            match field.r#type() {
                Type::Enum => out.push(type_name.to_owned()),
                Type::Message | Type::Group if Self::well_known(type_name).is_none() => {
                    if let Some(entry) = self.messages.get(type_name).filter(|m| m.is_map_entry) {
                        if let Some(value) = entry.descriptor.field.iter().find(|f| f.number() == 2)
                        {
                            let value_type = value.type_name().trim_start_matches('.');
                            if matches!(value.r#type(), Type::Enum | Type::Message)
                                && Self::well_known(value_type).is_none()
                            {
                                out.push(value_type.to_owned());
                            }
                        }
                    } else {
                        out.push(type_name.to_owned());
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// A self-contained JSON Schema (draft 2020-12) for one message: its own shape at the root
    /// and every type it reaches under `$defs`.
    pub fn message_schema(&self, name: &str) -> Option<Value> {
        let root = name.to_owned();
        let refs = |target: &str| {
            if target == root {
                "#".to_owned()
            } else {
                format!("#/$defs/{target}")
            }
        };
        let mut schema = self.message_body(name, &refs)?;
        let mut reached = BTreeSet::new();
        let mut queue: VecDeque<String> = self.direct_refs(name).into();
        while let Some(next) = queue.pop_front() {
            if next == name || !reached.insert(next.clone()) {
                continue;
            }
            queue.extend(self.direct_refs(&next));
        }
        let mut defs = Map::new();
        for def in &reached {
            if let Some(body) = self
                .message_body(def, &refs)
                .or_else(|| self.enum_body(def))
            {
                defs.insert(def.clone(), body);
            }
        }
        let object = schema.as_object_mut()?;
        object.insert(
            "$schema".into(),
            json!("https://json-schema.org/draft/2020-12/schema"),
        );
        object.insert(
            "$id".into(),
            json!(format!(
                "{SCHEMA_BASE_URI}{MESSAGES_DIR}/{name}.schema.json"
            )),
        );
        if !defs.is_empty() {
            object.insert("$defs".into(), Value::Object(defs));
        }
        Some(schema)
    }

    /// The OpenAPI 3.1 document of the whole HTTP/JSON surface.
    pub fn openapi(&self) -> Value {
        let refs = |target: &str| format!("#/components/schemas/{target}");
        let mut schemas = Map::new();
        for name in self.message_names() {
            if let Some(body) = self.message_body(name, &refs) {
                schemas.insert(name.to_owned(), body);
            }
        }
        for name in self.enums.keys().filter(|n| n.starts_with("bee.")) {
            if let Some(body) = self.enum_body(name) {
                schemas.insert(name.clone(), body);
            }
        }
        schemas.insert("connect.ErrorDetail".into(), json!({
            "type": "object",
            "title": "connect.ErrorDetail",
            "description": "A typed error detail. `bee.v1.ErrorDetail` is always present on bee errors.",
            "required": ["type", "value"],
            "properties": {
                "type": {"type": "string", "description": "Fully qualified message type, e.g. bee.v1.ErrorDetail."},
                "value": {"type": "string", "contentEncoding": "base64", "description": "Protobuf encoding of the detail, base64 without padding."},
                "debug": {"description": "proto3-JSON form of the detail, for JSON-only clients."}
            }
        }));
        schemas.insert("connect.Error".into(), json!({
            "type": "object",
            "title": "connect.Error",
            "description": "Connect error body (SPEC §12.2). The HTTP status follows the code.",
            "required": ["code"],
            "properties": {
                "code": {"type": "string", "enum": [
                    "canceled", "unknown", "invalid_argument", "deadline_exceeded", "not_found",
                    "already_exists", "permission_denied", "resource_exhausted",
                    "failed_precondition", "aborted", "out_of_range", "unimplemented",
                    "internal", "unavailable", "data_loss", "unauthenticated"
                ]},
                "message": {"type": "string"},
                "details": {"type": "array", "items": {"$ref": "#/components/schemas/connect.ErrorDetail"}}
            }
        }));

        let mut paths = Map::new();
        for method in METHODS {
            let service = self.services.get(method.service);
            let comment = service
                .and_then(|s| s.method_comments.get(method.method))
                .cloned()
                .unwrap_or_default();
            let (content_type, framing) = if method.kind.is_unary() {
                ("application/json", "")
            } else {
                (
                    "application/connect+json",
                    "\n\nStreaming: each message is enveloped (1 flags byte, 4-byte big-endian \
                     length, JSON). The response ends with an end-stream envelope (flags 0x02) \
                     holding `{\"error\"?, \"metadata\"?}`.",
                )
            };
            let kind = match method.kind {
                MethodKind::Unary => "unary",
                MethodKind::ServerStreaming => "server-streaming",
                MethodKind::ClientStreaming => "client-streaming",
                MethodKind::BidiStreaming => "bidi-streaming (half-duplex over HTTP/1.1)",
            };
            let summary = comment.lines().next().unwrap_or(method.method).to_owned();
            let description = format!("{comment}\n\nShape: {kind}.{framing}")
                .trim()
                .to_owned();
            let operation = json!({
                "operationId": format!("{}_{}", method.service.trim_start_matches("bee.v1."), method.method),
                "tags": [method.service],
                "summary": summary,
                "description": description,
                "parameters": [
                    {"name": "Connect-Protocol-Version", "in": "header", "required": false,
                     "schema": {"type": "string", "enum": ["1"]}},
                    {"name": "Connect-Timeout-Ms", "in": "header", "required": false,
                     "schema": {"type": "integer", "minimum": 0}}
                ],
                "requestBody": {
                    "required": true,
                    "content": {content_type: {"schema": {"$ref": refs(method.input_type)}}}
                },
                "responses": {
                    "200": {
                        "description": format!("{} message(s).", method.output_type),
                        "content": {content_type: {"schema": {"$ref": refs(method.output_type)}}}
                    },
                    "default": {
                        "description": "Connect error.",
                        "content": {"application/json": {"schema": {"$ref": "#/components/schemas/connect.Error"}}}
                    }
                }
            });
            paths.insert(method.path.to_owned(), json!({"post": operation}));
        }

        let tags: Vec<Value> = self
            .services
            .values()
            .filter(|s| s.name.starts_with("bee."))
            .map(|s| json!({"name": s.name, "description": s.comment}))
            .collect();

        json!({
            "openapi": "3.1.0",
            "info": {
                "title": "bee.v1",
                "version": "v1",
                "description": "HTTP/JSON surface of bee and Beehive in Connect-protocol shape \
                    (SPEC §12.1). Every RPC is `POST /bee.v1.<Service>/<Method>` with a \
                    proto3-JSON body. Generated from proto/bee/v1; do not edit."
            },
            "servers": [{"url": "/"}],
            "security": [{"bearer": []}],
            "tags": tags,
            "paths": Value::Object(paths),
            "components": {
                "schemas": Value::Object(schemas),
                "securitySchemes": {
                    "bearer": {"type": "http", "scheme": "bearer", "bearerFormat": "Biscuit"}
                }
            }
        })
    }

    /// Every generated file, keyed by its path relative to the repository root.
    pub fn generated_files(&self) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        files.insert(OPENAPI_PATH.to_owned(), render(&self.openapi()));
        let mut names = BTreeSet::new();
        for method in METHODS {
            names.insert(method.input_type);
            names.insert(method.output_type);
        }
        for name in names {
            if let Some(schema) = self.message_schema(name) {
                files.insert(
                    format!("{MESSAGES_DIR}/{name}.schema.json"),
                    render(&schema),
                );
            }
        }
        files
    }
}

fn with_description(schema: &mut Value, description: &str) {
    // A `description` next to `$ref` is valid in draft 2020-12 and OpenAPI 3.1.
    if let Some(object) = schema.as_object_mut() {
        object.insert("description".into(), json!(description));
    }
}

/// Pretty JSON with a trailing newline, the on-disk form of every generated file.
pub fn render(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}
