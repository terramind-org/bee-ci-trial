//! Cross-crate conformance, differential (vs git), reference-model and simulation tests.
//!
//! The contract lives in SPEC.md; see docs/PLAN.md for who builds this. [`parity`] holds the
//! three-form (API = CLI = MCP) conformance harness's server-independent parts; its runner is
//! the `three_forms` test, documented in `docs/spec/verification.md`. [`load`] holds the
//! scale tooling: the linearizability checker, the SPEC §16 report and the client the scale
//! tests and the load harness share.

pub mod load;
pub mod parity;
