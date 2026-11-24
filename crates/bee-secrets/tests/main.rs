//! The integration tests of `bee-secrets`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-secrets --test integration <file_stem>::`.

#[path = "config.rs"]
mod config;
#[path = "push_protection.rs"]
mod push_protection;
#[path = "robustness.rs"]
mod robustness;
#[path = "rules.rs"]
mod rules;
