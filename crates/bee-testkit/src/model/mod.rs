//! The reference model (SPEC §14.1): small, obviously correct implementations that
//! production is compared against by property tests.
//!
//! The model shares only data types with production (`ObjectId`, `RepoPath`, `DeltaEntry`,
//! `Tree` for encoding, `Expected`/`LandedEntry` for refs). Its algorithms are written
//! independently and favour clarity over speed: apply-delta edits a flat path map instead of
//! trees, the graph computes full ancestor sets, blame uses a quadratic LCS.
//!
//! - [`tree`]: apply-delta, exact diff, root tree id.
//! - [`refs`]: versioned CAS, ref log, landed index and land ordering.
//! - [`graph`]: ancestry, generations, first-parent chains, merge bases.
//! - [`blame`]: line attribution over a file's history.

pub mod blame;
pub mod graph;
pub mod refs;
pub mod tree;

pub use graph::Graph;
pub use refs::{LandOutcome, RefModel, RefModelError};
pub use tree::{FileMap, Leaf, ModelError};
