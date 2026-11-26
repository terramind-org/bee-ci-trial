//! The table of every `bee.v1` RPC, generated from the descriptors by build.rs.

use crate::transcode::{TranscodeFn, json_to_proto, proto_to_json};

/// Streaming shape of an RPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MethodKind {
    /// One request, one response.
    Unary,
    /// One request, a stream of responses.
    ServerStreaming,
    /// A stream of requests, one response.
    ClientStreaming,
    /// Streams in both directions.
    BidiStreaming,
}

impl MethodKind {
    /// True for [`MethodKind::Unary`].
    pub fn is_unary(self) -> bool {
        self == Self::Unary
    }
}

/// One RPC: its names, shape and message transcoders.
#[derive(Clone, Copy, Debug)]
pub struct MethodDescriptor {
    /// Fully qualified service name, e.g. `bee.v1.ObjectService`.
    pub service: &'static str,
    /// Method name, e.g. `GetObjects`.
    pub method: &'static str,
    /// HTTP and gRPC path, `/<service>/<method>`.
    pub path: &'static str,
    /// Streaming shape.
    pub kind: MethodKind,
    /// Fully qualified request message name.
    pub input_type: &'static str,
    /// Fully qualified response message name.
    pub output_type: &'static str,
    /// Proto file that declares the service, e.g. `bee/v1/vcs.proto`.
    pub file: &'static str,
    /// Request: proto3-JSON to protobuf binary.
    pub request_json_to_proto: TranscodeFn,
    /// Request: protobuf binary to proto3-JSON.
    pub request_proto_to_json: TranscodeFn,
    /// Response: proto3-JSON to protobuf binary.
    pub response_json_to_proto: TranscodeFn,
    /// Response: protobuf binary to proto3-JSON.
    pub response_proto_to_json: TranscodeFn,
}

include!(concat!(env!("OUT_DIR"), "/bee_v1_methods.rs"));

/// Finds an RPC by its path, e.g. `/bee.v1.RefService/ListRefs`.
pub fn find_method(path: &str) -> Option<&'static MethodDescriptor> {
    METHODS
        .binary_search_by(|m| m.path.cmp(path))
        .ok()
        .map(|i| &METHODS[i])
}

/// Every service name, sorted, without duplicates.
pub fn service_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = METHODS.iter().map(|m| m.service).collect();
    names.dedup();
    names
}
