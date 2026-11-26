//! Generated protobuf types for `bee.v1` and the Connect HTTP/JSON surface.
//!
//! - [`v1`] holds every message, enum, tonic client and tonic server generated from
//!   `proto/bee/v1/*.proto`, with proto3-JSON serde impls on every message.
//! - [`methods`] lists every RPC with functions that transcode its messages between JSON and
//!   protobuf binary.
//! - [`errors`] is the SPEC §12.2 error contract: reasons, gRPC and Connect codes, retryability.
//! - [`connect`] is the HTTP/JSON gateway in Connect-protocol shape that `bee-server` mounts
//!   next to its gRPC services (SPEC §12.1).
//! - [`schema`] generates the OpenAPI description and JSON Schemas from the descriptors.

pub mod connect;
pub mod envelope;
pub mod errors;
pub mod methods;
pub mod schema;
pub mod transcode;

/// Everything generated from `proto/bee/v1`. Doc comments come from the proto comments.
#[allow(
    missing_docs,
    clippy::all,
    rustdoc::bare_urls,
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_html_tags
)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/bee.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/bee.v1.serde.rs"));
}

/// The encoded `FileDescriptorSet` of every `bee.v1` file and its imports, with source info.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/bee_v1_descriptor.bin"));
