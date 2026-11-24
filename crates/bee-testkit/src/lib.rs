//! Test fixtures, in-memory stores, deterministic clock and fault injection.
//!
//! - [`clock::TestClock`]: a clock that moves only when told.
//! - [`rng::TestRng`]: a seeded, frozen-sequence RNG.
//! - [`fixtures`]: [`fixtures::TestEnv`] (in-memory stores wrapped with fault injection, a
//!   writer, shared access counters) and builders for repos and commits that go through the
//!   real write path.
//! - [`model`]: the independent reference model (SPEC §14.1).
//!
//! The in-memory stores, fault points, fsck and GC themselves live in `bee-store`, so the
//! SQL and S3 implementations can be checked by the same oracles. This crate's tests are the
//! store's crash, concurrency, consistency and model-comparison suites.

pub mod clock;
pub mod fixtures;
pub mod linearizability;
pub mod model;
pub mod rng;

pub use clock::TestClock;
pub use fixtures::{Change, CommitSpec, TestEnv, TestRepo, ref_name};
pub use rng::TestRng;

#[cfg(test)]
mod tests;
