//! The wire contract as a whole: every RPC is routable and transcodable, proto3-JSON follows
//! the mapping, and the protos only ever grow (AGENTS.md rule 3).

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::methods::{METHODS, MethodKind, find_method, service_names};
use bee_proto::v1::*;
use prost::Message;
use prost_types::{DescriptorProto, FileDescriptorSet};
use serde_json::{Value, json};

fn current() -> FileDescriptorSet {
    FileDescriptorSet::decode(bee_proto::FILE_DESCRIPTOR_SET).expect("descriptor set")
}

#[test]
fn method_table_matches_descriptors() {
    let set = current();
    let mut from_descriptors = BTreeSet::new();
    for file in set.file.iter().filter(|f| f.package() == "bee.v1") {
        for service in &file.service {
            for method in &service.method {
                from_descriptors.insert(format!("/bee.v1.{}/{}", service.name(), method.name()));
            }
        }
    }
    let from_table: BTreeSet<String> = METHODS.iter().map(|m| m.path.to_owned()).collect();
    assert_eq!(from_table, from_descriptors);
    assert_eq!(from_table.len(), METHODS.len(), "paths are unique");
    assert!(METHODS.windows(2).all(|w| w[0].path < w[1].path), "sorted");
    for method in METHODS {
        assert_eq!(find_method(method.path).map(|m| m.path), Some(method.path));
        assert_eq!(
            method.path,
            format!("/{}/{}", method.service, method.method)
        );
    }
    assert!(find_method("/bee.v1.Nope/Nope").is_none());
    assert!(service_names().len() >= 40, "{:?}", service_names());
}

#[test]
fn every_message_transcodes_both_ways() {
    for method in METHODS {
        for (to_proto, to_json, name) in [
            (
                method.request_json_to_proto,
                method.request_proto_to_json,
                method.input_type,
            ),
            (
                method.response_json_to_proto,
                method.response_proto_to_json,
                method.output_type,
            ),
        ] {
            let proto = to_proto(b"{}").unwrap_or_else(|e| panic!("{name}: {e}"));
            let json = to_json(&proto).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(json, b"{}", "{name}: default message is the empty object");
            assert!(to_proto(b"[]").is_err(), "{name}: arrays are not messages");
        }
    }
}

#[test]
fn new_rpcs_are_unary_or_server_streaming() {
    // Browsers (connect-web) cannot send request streams; only UploadObjects predates this.
    for method in METHODS {
        if matches!(
            method.kind,
            MethodKind::ClientStreaming | MethodKind::BidiStreaming
        ) {
            assert_eq!(method.path, "/bee.v1.WriteService/UploadObjects");
        }
    }
}

#[test]
fn proto3_json_mapping() {
    let change = ChangeRequest {
        id: "01J".into(),
        number: 42,
        state: ChangeState::Open as i32,
        draft: true,
        change_ids: vec![ChangeId {
            raw: vec![0xff, 0x00],
        }],
        created_at: Some(pbjson_types::Timestamp {
            seconds: 1_760_000_000,
            nanos: 0,
        }),
        auto_merge: Some(AutoMerge {
            method: LandMethod::Squash as i32,
            ..AutoMerge::default()
        }),
        ..ChangeRequest::default()
    };
    let value = serde_json::to_value(&change).expect("serialize");
    assert_eq!(
        value,
        json!({
            "id": "01J",
            "number": "42",
            "state": "CHANGE_STATE_OPEN",
            "draft": true,
            "changeIds": [{"raw": "/wA="}],
            "createdAt": "2025-10-09T08:53:20+00:00",
            "autoMerge": {"method": "LAND_METHOD_SQUASH"}
        })
    );
    // Parsers accept original names, numeric enums, numbers for 64-bit ints, and ignore
    // unknown fields.
    let parsed: ChangeRequest = serde_json::from_value(json!({
        "id": "01J",
        "number": 42,
        "state": 1,
        "draft": true,
        "change_ids": [{"raw": "/wA="}],
        "createdAt": "2025-10-09T08:53:20Z",
        "auto_merge": {"method": "LAND_METHOD_SQUASH"},
        "fieldFromTheFuture": {"x": 1}
    }))
    .expect("parse");
    assert_eq!(parsed, change);
    // Oneofs and proto3 optional.
    let repo: RepoRef = serde_json::from_value(json!({"fullName": "acme/api"})).expect("oneof");
    assert_eq!(repo.repo, Some(repo_ref::Repo::FullName("acme/api".into())));
    let update: UpdateOrgRequest =
        serde_json::from_value(json!({"org": "acme", "display": ""})).expect("optional");
    assert_eq!(
        update.display.as_deref(),
        Some(""),
        "present-but-empty is kept"
    );
    assert_eq!(update.description, None);
}

#[test]
fn error_detail_json_names_reason() {
    let mut detail = bee_proto::errors::error_detail(ErrorReason::MergeConflict, "conflict");
    detail.payload = Some(error_detail::Payload::Conflicts(ConflictSet::default()));
    let value: Value = serde_json::to_value(&detail).expect("json");
    assert_eq!(value["reason"], "MERGE_CONFLICT");
    assert_eq!(value["conflicts"], json!({}));
}

/// Everything about a field that may never change once published.
fn field_shape(f: &prost_types::FieldDescriptorProto) -> (String, i32, i32, String, Option<i32>) {
    (
        f.name().to_owned(),
        f.r#type() as i32,
        f.label() as i32,
        f.type_name().to_owned(),
        f.oneof_index,
    )
}

fn collect_messages(
    prefix: &str,
    messages: &[DescriptorProto],
    out: &mut BTreeMap<String, DescriptorProto>,
) {
    for message in messages {
        let name = format!("{prefix}.{}", message.name());
        collect_messages(&name, &message.nested_type, out);
        out.insert(name, message.clone());
    }
}

#[test]
fn protos_only_grow_from_the_baseline() {
    let baseline =
        FileDescriptorSet::decode(include_bytes!("baseline/bee-v1-697c80e.binpb").as_slice())
            .expect("baseline");
    let now = current();
    let index = |set: &FileDescriptorSet| {
        let mut messages = BTreeMap::new();
        let mut enums = BTreeMap::new();
        let mut services = BTreeMap::new();
        for file in set.file.iter().filter(|f| f.package() == "bee.v1") {
            collect_messages(".bee.v1", &file.message_type, &mut messages);
            for e in &file.enum_type {
                enums.insert(e.name().to_owned(), e.clone());
            }
            for s in &file.service {
                services.insert(s.name().to_owned(), s.clone());
            }
        }
        (messages, enums, services)
    };
    let (old_messages, old_enums, old_services) = index(&baseline);
    let (new_messages, new_enums, new_services) = index(&now);

    for (name, old) in &old_messages {
        let new = new_messages
            .get(name)
            .unwrap_or_else(|| panic!("message {name} removed"));
        for field in &old.field {
            let current = new
                .field
                .iter()
                .find(|f| f.number() == field.number())
                .unwrap_or_else(|| panic!("{name} field {} removed", field.number()));
            assert_eq!(
                field_shape(current),
                field_shape(field),
                "{name} field {} changed",
                field.number()
            );
        }
        for (i, oneof) in old.oneof_decl.iter().enumerate() {
            assert_eq!(new.oneof_decl.get(i).map(|o| o.name()), Some(oneof.name()));
        }
    }
    for (name, old) in &old_enums {
        let new = new_enums
            .get(name)
            .unwrap_or_else(|| panic!("enum {name} removed"));
        for value in &old.value {
            assert!(
                new.value
                    .iter()
                    .any(|v| v.number() == value.number() && v.name() == value.name()),
                "{name}.{} changed",
                value.name()
            );
        }
    }
    for (name, old) in &old_services {
        let new = new_services
            .get(name)
            .unwrap_or_else(|| panic!("service {name} removed"));
        for method in &old.method {
            let current = new
                .method
                .iter()
                .find(|m| m.name() == method.name())
                .unwrap_or_else(|| panic!("{name}.{} removed", method.name()));
            assert_eq!(
                (
                    current.input_type(),
                    current.output_type(),
                    current.client_streaming(),
                    current.server_streaming()
                ),
                (
                    method.input_type(),
                    method.output_type(),
                    method.client_streaming(),
                    method.server_streaming()
                ),
                "{name}.{} signature changed",
                method.name()
            );
        }
    }
}
