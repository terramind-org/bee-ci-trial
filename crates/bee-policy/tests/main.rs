//! The integration tests of `bee-policy`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-policy --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "access.rs"]
mod access;
#[path = "fuzz.rs"]
mod fuzz;
#[path = "land.rs"]
mod land;
#[path = "parse_epoch.rs"]
mod parse_epoch;
#[path = "rules_extra.rs"]
mod rules_extra;
