//! The command catalog: every `bee.v1` service and RPC with its request fields and comments,
//! read from the descriptor set embedded in `bee-proto`.
//!
//! Nouns are service names without `Service` in kebab-case (`WriteService` -> `write`); verbs are
//! method names in kebab-case (`LandChange` -> `land-change`).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use bee_proto::methods::{METHODS, MethodDescriptor, MethodKind};
use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, EnumDescriptorProto, FileDescriptorSet, SourceCodeInfo};

/// A protobuf scalar type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    /// `double`
    Double,
    /// `float`
    Float,
    /// `int32`, `sint32`, `sfixed32`
    Int32,
    /// `uint32`, `fixed32`
    Uint32,
    /// `int64`, `sint64`, `sfixed64`
    Int64,
    /// `uint64`, `fixed64`
    Uint64,
    /// `bool`
    Bool,
    /// `string`
    String,
    /// `bytes`
    Bytes,
}

impl Scalar {
    /// Name shown in help, e.g. `uint64`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Double => "double",
            Self::Float => "float",
            Self::Int32 => "int32",
            Self::Uint32 => "uint32",
            Self::Int64 => "int64",
            Self::Uint64 => "uint64",
            Self::Bool => "bool",
            Self::String => "string",
            Self::Bytes => "bytes",
        }
    }
}

/// The value type of a field (ignoring `repeated`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A scalar.
    Scalar(Scalar),
    /// An enum, by fully qualified name without the leading dot.
    Enum(String),
    /// A message, by fully qualified name without the leading dot.
    Message(String),
}

/// How many values a field holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cardinality {
    /// One value.
    Single,
    /// A list.
    Repeated,
    /// A `map<K, V>`: the key kind (always a scalar) and the value kind.
    Map(Scalar, Kind),
}

/// One field of a message.
#[derive(Clone, Debug)]
pub struct Field {
    /// Proto name, e.g. `change_request_id`.
    pub name: String,
    /// proto3-JSON name, e.g. `changeRequestId`.
    pub json_name: String,
    /// Value type.
    pub kind: Kind,
    /// Single, repeated or map.
    pub cardinality: Cardinality,
    /// Name of the (non-synthetic) oneof the field belongs to, if any.
    pub oneof: Option<String>,
    /// Comment from the proto file, possibly empty.
    pub comment: String,
}

/// One message type.
#[derive(Clone, Debug)]
pub struct MessageType {
    /// Fully qualified name, e.g. `bee.v1.LandRequest`.
    pub name: String,
    /// Fields in declaration order.
    pub fields: Vec<Field>,
    /// Comment from the proto file, possibly empty.
    pub comment: String,
}

impl MessageType {
    /// The field whose JSON or proto name is `key`.
    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields
            .iter()
            .find(|f| f.json_name == key || f.name == key)
    }
}

/// One enum type.
#[derive(Clone, Debug)]
pub struct EnumType {
    /// Fully qualified name, e.g. `bee.v1.LandMethod`.
    pub name: String,
    /// Value names and numbers in declaration order.
    pub values: Vec<(String, i32)>,
}

impl EnumType {
    /// The common `SCREAMING_SNAKE_` prefix of the values (`LAND_METHOD_`), from the enum name.
    pub fn prefix(&self) -> String {
        let short = self.name.rsplit('.').next().unwrap_or(&self.name);
        format!("{}_", kebab(short).replace('-', "_").to_ascii_uppercase())
    }

    /// The CLI spelling of a value: prefix removed, lowercase, `-` for `_` (`merge-commit`).
    pub fn short_name(&self, value: &str) -> String {
        value
            .strip_prefix(&self.prefix())
            .unwrap_or(value)
            .to_ascii_lowercase()
            .replace('_', "-")
    }

    /// Values a user would pick: every value except `*_UNSPECIFIED` (number 0).
    pub fn choices(&self) -> Vec<String> {
        self.values
            .iter()
            .filter(|(_, number)| *number != 0)
            .map(|(name, _)| self.short_name(name))
            .collect()
    }

    /// Resolves a full name, a short name (any case) or a number to the full value name.
    pub fn resolve(&self, input: &str) -> Option<String> {
        let wanted = input.trim();
        if let Ok(number) = wanted.parse::<i32>() {
            return self
                .values
                .iter()
                .find(|(_, n)| *n == number)
                .map(|(name, _)| name.clone());
        }
        let normalized = wanted.to_ascii_lowercase().replace('_', "-");
        self.values
            .iter()
            .find(|(name, _)| name == wanted || self.short_name(name) == normalized)
            .map(|(name, _)| name.clone())
    }
}

/// One RPC as a CLI verb.
#[derive(Clone, Debug)]
pub struct Verb {
    /// Kebab-case verb, e.g. `land-change`.
    pub name: String,
    /// The RPC.
    pub method: &'static MethodDescriptor,
    /// Comment from the proto file, possibly empty.
    pub comment: String,
}

impl Verb {
    /// One-line summary: the first sentence of the comment, or a generated line.
    pub fn summary(&self) -> String {
        first_line(&self.comment).unwrap_or_else(|| {
            let words = self.name.replace('-', " ");
            let mut chars = words.chars();
            let sentence = match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => words.clone(),
            };
            format!(
                "{sentence} ({} -> {}).",
                short_type(self.method.input_type),
                short_type(self.method.output_type)
            )
        })
    }

    /// True for server-streaming RPCs that stay open for new events (subscriptions, tails,
    /// watches); these take `--follow`.
    pub fn is_event_stream(&self) -> bool {
        self.method.kind == MethodKind::ServerStreaming
            && ["Subscribe", "Stream", "Watch", "Tail", "Acquire"]
                .iter()
                .any(|prefix| self.method.method.starts_with(prefix))
    }

    /// Shape as shown in help and the agent index.
    pub fn shape(&self) -> &'static str {
        kind_name(self.method.kind)
    }
}

/// Name of a streaming shape: `unary`, `server-streaming`, ...
pub fn kind_name(kind: MethodKind) -> &'static str {
    match kind {
        MethodKind::Unary => "unary",
        MethodKind::ServerStreaming => "server-streaming",
        MethodKind::ClientStreaming => "client-streaming",
        MethodKind::BidiStreaming => "bidi-streaming",
    }
}

/// One service as a CLI noun.
#[derive(Clone, Debug)]
pub struct Noun {
    /// Kebab-case noun, e.g. `write`.
    pub name: String,
    /// Fully qualified service name, e.g. `bee.v1.WriteService`.
    pub service: String,
    /// Comment from the proto file, possibly empty.
    pub comment: String,
    /// Verbs sorted by name.
    pub verbs: Vec<Verb>,
}

impl Noun {
    /// One-line summary for `bee --help`.
    pub fn summary(&self) -> String {
        first_line(&self.comment)
            .unwrap_or_else(|| format!("{} operations.", short_type(&self.service)))
    }

    /// The verb called `name`.
    pub fn verb(&self, name: &str) -> Option<&Verb> {
        self.verbs.iter().find(|v| v.name == name)
    }
}

/// Every noun, verb, message and enum.
#[derive(Debug, Default)]
pub struct Catalog {
    nouns: Vec<Noun>,
    messages: BTreeMap<String, MessageType>,
    enums: BTreeMap<String, EnumType>,
}

/// The catalog of the embedded descriptor set, built once per process.
pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(Catalog::load)
}

impl Catalog {
    fn load() -> Self {
        // The descriptor set is produced by bee-proto's build script from the same protos
        // as METHODS; a decode failure is a build defect, not a runtime condition.
        let set = FileDescriptorSet::decode(bee_proto::FILE_DESCRIPTOR_SET)
            .unwrap_or_else(|e| panic!("embedded descriptor set does not decode: {e}"));
        let mut catalog = Self::default();
        let mut service_comments: BTreeMap<String, (String, BTreeMap<String, String>)> =
            BTreeMap::new();
        let mut map_entries: BTreeMap<String, DescriptorProto> = BTreeMap::new();
        let mut raw_messages: Vec<(String, DescriptorProto, Vec<i32>, usize)> = Vec::new();
        let mut comment_tables = Vec::new();
        for file in &set.file {
            let comments = comments_by_path(file.source_code_info.as_ref());
            let table = comment_tables.len();
            let package = file.package().to_owned();
            for (i, message) in file.message_type.iter().enumerate() {
                collect_messages(
                    &package,
                    message,
                    vec![4, i as i32],
                    table,
                    &mut raw_messages,
                    &mut map_entries,
                    &mut catalog.enums,
                );
            }
            for e in &file.enum_type {
                catalog
                    .enums
                    .insert(format!("{package}.{}", e.name()), enum_type(&package, e));
            }
            for (i, service) in file.service.iter().enumerate() {
                let path = vec![6, i as i32];
                let methods = service
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
                service_comments.insert(
                    format!("{package}.{}", service.name()),
                    (comments.get(&path).cloned().unwrap_or_default(), methods),
                );
            }
            comment_tables.push(comments);
        }
        for (name, descriptor, path, table) in raw_messages {
            let comments = &comment_tables[table];
            let message = message_type(&name, &descriptor, &path, comments, &map_entries);
            catalog.messages.insert(name, message);
        }
        let mut nouns: BTreeMap<String, Noun> = BTreeMap::new();
        for method in METHODS {
            let (service_comment, method_comments) = service_comments
                .get(method.service)
                .cloned()
                .unwrap_or_default();
            let noun = noun_name(method.service);
            let entry = nouns.entry(noun.clone()).or_insert_with(|| Noun {
                name: noun,
                service: method.service.to_owned(),
                comment: service_comment,
                verbs: Vec::new(),
            });
            entry.verbs.push(Verb {
                name: kebab(method.method),
                method,
                comment: method_comments
                    .get(method.method)
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        for noun in nouns.values_mut() {
            noun.verbs.sort_by(|a, b| a.name.cmp(&b.name));
        }
        catalog.nouns = nouns.into_values().collect();
        catalog
    }

    /// Every noun, sorted by name.
    pub fn nouns(&self) -> &[Noun] {
        &self.nouns
    }

    /// The noun called `name`.
    pub fn noun(&self, name: &str) -> Option<&Noun> {
        self.nouns.iter().find(|n| n.name == name)
    }

    /// A message type by fully qualified name.
    pub fn message(&self, name: &str) -> Option<&MessageType> {
        self.messages.get(name)
    }

    /// An enum type by fully qualified name.
    pub fn enum_type(&self, name: &str) -> Option<&EnumType> {
        self.enums.get(name)
    }

    /// Every `(noun, verb)` pair, in noun then verb order.
    pub fn commands(&self) -> impl Iterator<Item = (&Noun, &Verb)> {
        self.nouns
            .iter()
            .flat_map(|noun| noun.verbs.iter().map(move |verb| (noun, verb)))
    }
}

/// Walks a message and its nested types, recording messages, map entries and nested enums.
fn collect_messages(
    scope: &str,
    message: &DescriptorProto,
    path: Vec<i32>,
    table: usize,
    out: &mut Vec<(String, DescriptorProto, Vec<i32>, usize)>,
    map_entries: &mut BTreeMap<String, DescriptorProto>,
    enums: &mut BTreeMap<String, EnumType>,
) {
    let name = format!("{scope}.{}", message.name());
    for (j, nested) in message.nested_type.iter().enumerate() {
        let mut p = path.clone();
        p.extend([3, j as i32]);
        collect_messages(&name, nested, p, table, out, map_entries, enums);
    }
    for e in &message.enum_type {
        enums.insert(format!("{name}.{}", e.name()), enum_type(&name, e));
    }
    if message.options.as_ref().is_some_and(|o| o.map_entry()) {
        map_entries.insert(name, message.clone());
    } else {
        out.push((name, message.clone(), path, table));
    }
}

fn enum_type(scope: &str, e: &EnumDescriptorProto) -> EnumType {
    EnumType {
        name: format!("{scope}.{}", e.name()),
        values: e
            .value
            .iter()
            .map(|v| (v.name().to_owned(), v.number()))
            .collect(),
    }
}

fn scalar(t: Type) -> Option<Scalar> {
    Some(match t {
        Type::Double => Scalar::Double,
        Type::Float => Scalar::Float,
        Type::Int32 | Type::Sint32 | Type::Sfixed32 => Scalar::Int32,
        Type::Uint32 | Type::Fixed32 => Scalar::Uint32,
        Type::Int64 | Type::Sint64 | Type::Sfixed64 => Scalar::Int64,
        Type::Uint64 | Type::Fixed64 => Scalar::Uint64,
        Type::Bool => Scalar::Bool,
        Type::String => Scalar::String,
        Type::Bytes => Scalar::Bytes,
        Type::Enum | Type::Message | Type::Group => return None,
    })
}

fn field_kind(field: &prost_types::FieldDescriptorProto) -> Kind {
    let type_name = field.type_name().trim_start_matches('.').to_owned();
    match field.r#type() {
        Type::Enum => Kind::Enum(type_name),
        Type::Message | Type::Group => Kind::Message(type_name),
        other => Kind::Scalar(scalar(other).unwrap_or(Scalar::String)),
    }
}

fn message_type(
    name: &str,
    descriptor: &DescriptorProto,
    path: &[i32],
    comments: &BTreeMap<Vec<i32>, String>,
    map_entries: &BTreeMap<String, DescriptorProto>,
) -> MessageType {
    let fields = descriptor
        .field
        .iter()
        .enumerate()
        .map(|(k, field)| {
            let mut p = path.to_vec();
            p.extend([2, k as i32]);
            let kind = field_kind(field);
            let cardinality = match (&kind, field.label()) {
                (Kind::Message(type_name), Label::Repeated) => {
                    match map_entries.get(type_name.as_str()) {
                        Some(entry) => {
                            let key = entry
                                .field
                                .iter()
                                .find(|f| f.number() == 1)
                                .and_then(|f| scalar(f.r#type()))
                                .unwrap_or(Scalar::String);
                            let value = entry
                                .field
                                .iter()
                                .find(|f| f.number() == 2)
                                .map(field_kind)
                                .unwrap_or(Kind::Scalar(Scalar::String));
                            Cardinality::Map(key, value)
                        }
                        None => Cardinality::Repeated,
                    }
                }
                (_, Label::Repeated) => Cardinality::Repeated,
                _ => Cardinality::Single,
            };
            let oneof = field
                .oneof_index
                .filter(|_| !field.proto3_optional())
                .and_then(|i| descriptor.oneof_decl.get(i as usize))
                .map(|o| o.name().to_owned());
            Field {
                name: field.name().to_owned(),
                json_name: field.json_name().to_owned(),
                kind,
                cardinality,
                oneof,
                comment: comments.get(&p).cloned().unwrap_or_default(),
            }
        })
        .collect();
    MessageType {
        name: name.to_owned(),
        fields,
        comment: comments.get(path).cloned().unwrap_or_default(),
    }
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

/// The first sentence (or line) of a comment, ending in a period.
pub fn first_line(comment: &str) -> Option<String> {
    let flat = comment.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    let end = flat
        .match_indices(". ")
        .next()
        .map_or(flat.len(), |(i, _)| i + 1);
    let mut line = flat[..end].to_owned();
    if !line.ends_with('.') {
        line.push('.');
    }
    Some(line)
}

/// `bee.v1.LandRequest` -> `LandRequest`.
pub fn short_type(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// The noun of a service: `bee.v1.WriteService` -> `write`.
pub fn noun_name(service: &str) -> String {
    let short = short_type(service);
    kebab(short.strip_suffix("Service").unwrap_or(short))
}

/// CamelCase to kebab-case, keeping acronyms together: `WhoAmI` -> `who-am-i`,
/// `GetSBOMExport` -> `get-sbom-export`, `GitLFSBatch` -> `git-lfs-batch`.
pub fn kebab(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            out.push('-');
            continue;
        }
        if c.is_ascii_uppercase() && i > 0 {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_lower)
            {
                out.push('-');
            }
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_keeps_acronyms_together() {
        assert_eq!(kebab("WhoAmI"), "who-am-i");
        assert_eq!(kebab("GetSBOMExport"), "get-sbom-export");
        assert_eq!(kebab("GitLFSBatch"), "git-lfs-batch");
        assert_eq!(kebab("LandChange"), "land-change");
        assert_eq!(kebab("GetV2Thing"), "get-v2-thing");
        assert_eq!(kebab("change_request_id"), "change-request-id");
        assert_eq!(noun_name("bee.v1.WriteService"), "write");
        assert_eq!(noun_name("bee.v1.InsightsService"), "insights");
    }

    #[test]
    fn catalog_has_every_rpc_once() {
        let catalog = catalog();
        assert_eq!(catalog.commands().count(), METHODS.len());
        for noun in catalog.nouns() {
            let mut names: Vec<&str> = noun.verbs.iter().map(|v| v.name.as_str()).collect();
            names.dedup();
            assert_eq!(
                names.len(),
                noun.verbs.len(),
                "duplicate verb in {}",
                noun.name
            );
        }
        let land = catalog
            .noun("write")
            .and_then(|n| n.verb("land"))
            .expect("land");
        assert_eq!(land.method.path, "/bee.v1.WriteService/Land");
        let request = catalog.message(land.method.input_type).expect("request");
        assert!(request.field("repo").is_some());
        assert!(request.field("change_request_id").is_some());
    }

    #[test]
    fn enum_spellings() {
        let method = catalog().enum_type("bee.v1.LandMethod").expect("enum");
        assert_eq!(method.prefix(), "LAND_METHOD_");
        assert_eq!(method.choices(), ["rebase", "squash", "merge-commit"]);
        assert_eq!(
            method.resolve("merge-commit").as_deref(),
            Some("LAND_METHOD_MERGE_COMMIT")
        );
        assert_eq!(
            method.resolve("LAND_METHOD_SQUASH").as_deref(),
            Some("LAND_METHOD_SQUASH")
        );
        assert_eq!(method.resolve("1").as_deref(), Some("LAND_METHOD_REBASE"));
        assert_eq!(method.resolve("nope"), None);
    }

    #[test]
    fn first_line_is_one_sentence() {
        assert_eq!(
            first_line("Revokes a token id. And more.\nLines.").as_deref(),
            Some("Revokes a token id.")
        );
        assert_eq!(first_line("No period").as_deref(), Some("No period."));
        assert_eq!(first_line("  "), None);
    }
}
