//! Generates Rust for every file in `proto/bee/v1`: prost messages, tonic clients and servers,
//! pbjson (proto3-JSON) serde impls, and a table of every RPC for the Connect gateway.
//!
//! The protoc binary comes from `protoc-bin-vendored`, so builds do not depend on whatever
//! protoc happens to be on PATH. New `.proto` files are picked up by the directory scan.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use prost::Message as _;
use prost_types::FileDescriptorSet;
use prost_types::field_descriptor_proto::Type;

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let proto_root = manifest_dir.join("../../proto");
    let proto_dir = proto_root.join("bee/v1");

    let mut protos: Vec<PathBuf> = fs::read_dir(&proto_dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "proto"))
        .collect();
    protos.sort();
    println!("cargo:rerun-if-changed={}", proto_dir.display());
    for proto in &protos {
        println!("cargo:rerun-if-changed={}", proto.display());
    }

    let descriptor_path = out_dir.join("bee_v1_descriptor.bin");
    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);
    config.file_descriptor_set_path(&descriptor_path);
    config.compile_well_known_types();
    config.extern_path(".google.protobuf", "::pbjson_types");
    // A post embedded inline would grow every ChangeRequest and Event by the size of a Post,
    // and the futures that hold several of them in debug builds then overflow test-thread
    // stacks; boxed, they grow by one pointer.
    config.boxed(".bee.v1.ChangeRequest.post");
    config.boxed(".bee.v1.Event.body.post");
    // The brief (SPEC §11.9) is boxed for the same reason: a ChangeRequest is held by value in
    // many futures and events.
    config.boxed(".bee.v1.ChangeRequest.brief");

    let includes = [proto_root.clone(), protoc_bin_vendored::include_path()?];
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .build_transport(false)
        .emit_rerun_if_changed(false)
        .compile_with_config(config, &protos, &includes)?;

    let descriptor_bytes = fs::read(&descriptor_path)?;
    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_bytes)?
        .ignore_unknown_fields()
        .build(&[".bee"])?;

    let set = FileDescriptorSet::decode(descriptor_bytes.as_slice())?;
    fs::write(out_dir.join("bee_v1_methods.rs"), method_table(&set)?)?;
    Ok(())
}

/// Rust path of a fully qualified proto message name, relative to the crate root.
fn rust_type(proto_name: &str) -> Result<String, Box<dyn Error>> {
    if let Some(name) = proto_name.strip_prefix(".bee.v1.") {
        if name.contains('.') {
            return Err(format!("nested message {proto_name} used as an RPC type").into());
        }
        Ok(format!("crate::v1::{name}"))
    } else if let Some(name) = proto_name.strip_prefix(".google.protobuf.") {
        Ok(format!("::pbjson_types::{name}"))
    } else {
        Err(format!("unsupported RPC type {proto_name}").into())
    }
}

/// Emits `METHODS`, one `MethodDescriptor` per RPC, sorted by path.
fn method_table(set: &FileDescriptorSet) -> Result<String, Box<dyn Error>> {
    let mut rows = BTreeMap::new();
    for file in &set.file {
        if file.package() != "bee.v1" {
            continue;
        }
        for service in &file.service {
            let service_name = format!("bee.v1.{}", service.name());
            for method in &service.method {
                let path = format!("/{service_name}/{}", method.name());
                let kind = match (method.client_streaming(), method.server_streaming()) {
                    (false, false) => "Unary",
                    (false, true) => "ServerStreaming",
                    (true, false) => "ClientStreaming",
                    (true, true) => "BidiStreaming",
                };
                let input = rust_type(method.input_type())?;
                let output = rust_type(method.output_type())?;
                let mut row = String::new();
                writeln!(row, "    MethodDescriptor {{")?;
                writeln!(row, "        service: {service_name:?},")?;
                writeln!(row, "        method: {:?},", method.name())?;
                writeln!(row, "        path: {path:?},")?;
                writeln!(row, "        kind: MethodKind::{kind},")?;
                writeln!(row, "        input_type: {:?},", &method.input_type()[1..])?;
                writeln!(
                    row,
                    "        output_type: {:?},",
                    &method.output_type()[1..]
                )?;
                writeln!(row, "        file: {:?},", file.name())?;
                writeln!(
                    row,
                    "        request_json_to_proto: json_to_proto::<{input}>,"
                )?;
                writeln!(
                    row,
                    "        request_proto_to_json: proto_to_json::<{input}>,"
                )?;
                writeln!(
                    row,
                    "        response_json_to_proto: json_to_proto::<{output}>,"
                )?;
                writeln!(
                    row,
                    "        response_proto_to_json: proto_to_json::<{output}>,"
                )?;
                writeln!(row, "    }},")?;
                if rows.insert(path.clone(), row).is_some() {
                    return Err(format!("duplicate RPC path {path}").into());
                }
            }
        }
    }
    let mut out = String::from(
        "/// Every RPC in `bee.v1`, sorted by path. Generated by build.rs from the descriptors.\n\
         pub static METHODS: &[MethodDescriptor] = &[\n",
    );
    for row in rows.values() {
        out.push_str(row);
    }
    out.push_str("];\n");
    // Fail the build early if a descriptor uses a type the JSON schema generator cannot map.
    for file in &set.file {
        for message in &file.message_type {
            for field in &message.field {
                if field.r#type() == Type::Group {
                    return Err(format!("group field in {}", message.name()).into());
                }
            }
        }
    }
    Ok(out)
}
