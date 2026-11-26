//! The integration tests of `bee-search`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-search --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "incremental.rs"]
mod incremental;
#[path = "path_index.rs"]
mod path_index;
#[path = "perf.rs"]
mod perf;
#[path = "permissions.rs"]
mod permissions;
#[path = "property.rs"]
mod property;
#[path = "query_golden.rs"]
mod query_golden;
#[path = "ranking.rs"]
mod ranking;
#[path = "scenarios.rs"]
mod scenarios;
