//! Scale and load tooling (plan task W14): the linearizability checker, latency statistics,
//! the SPEC §16 report and a small gRPC client used by the scale tests and the load harness.
//!
//! - [`history`]: Wing–Gong style linearizability checking of recorded histories against the
//!   reference model of refs (SPEC §14.7).
//! - [`stats`]: nearest-rank latency summaries.
//! - [`report`]: every SPEC §16 target, judged against measurements, rendered as Markdown.
//! - [`client`]: typed calls against a running server (upload, commit, land, refs, history,
//!   events, tokens) shared by `tests/{linearizability,complexity,many_agents,faults}.rs` and
//!   the `load` example.
//!
//! The harness itself is the `load` example (`src/load/main.rs`), because it starts an
//! in-process `bee-server`, which sits in a higher layer than this crate:
//!
//! ```text
//! cargo build --release -p bee-conformance --example load
//! target/release/examples/load --landers 1000 [--pg]
//! ```

pub mod client;
pub mod history;
pub mod report;
pub mod stats;
