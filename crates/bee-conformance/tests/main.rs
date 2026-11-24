//! The integration tests of `bee-conformance`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-conformance --test integration <file_stem>::`.

#[path = "adopt.rs"]
mod adopt;
#[path = "complexity.rs"]
mod complexity;
#[path = "faults.rs"]
mod faults;
#[path = "forge_events.rs"]
mod forge_events;
#[path = "github_import.rs"]
mod github_import;
#[path = "linearizability.rs"]
mod linearizability;
#[path = "many_agents.rs"]
mod many_agents;
#[path = "posts.rs"]
mod posts;
#[path = "public_read.rs"]
mod public_read;
#[path = "repo_scoped_tokens.rs"]
mod repo_scoped_tokens;
mod scale_support;
#[path = "spec_records.rs"]
mod spec_records;
