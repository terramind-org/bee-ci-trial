//! The integration tests of `bee-history`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-history --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "graph.rs"]
mod graph;
#[path = "merge.rs"]
mod merge;
#[path = "queries.rs"]
mod queries;
