//! The integration tests of `bee-merge`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-merge --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "git_differential.rs"]
mod git_differential;
#[path = "golden.rs"]
mod golden;
#[path = "properties.rs"]
mod properties;
#[path = "tree.rs"]
mod tree;
