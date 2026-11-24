//! The integration tests of `bee-github`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-github --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "cli.rs"]
mod cli;
#[path = "github_import.rs"]
mod github_import;
#[path = "hub.rs"]
mod hub;
