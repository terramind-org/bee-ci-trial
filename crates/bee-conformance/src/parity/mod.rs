//! Three-form conformance (PRODUCT §3.2): every served RPC is called as the HTTP/JSON API
//! (gRPC sampled), as `bee <noun> <verb> --json`, and as the MCP tool `<service>_<method>`,
//! and the three outcomes must be equal after normalising volatile values.
//!
//! This module holds the server-independent parts: fixtures and their templates, naming
//! rules, outcome normalisation, contract facts and the coverage report. The runner that
//! starts servers and drives the forms is `tests/three_forms/` (see `docs/spec/verification.md`).

pub mod contract;
pub mod fixture;
pub mod naming;
pub mod normalize;
pub mod report;
pub mod template;
