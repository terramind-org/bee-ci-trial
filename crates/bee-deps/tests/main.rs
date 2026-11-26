//! The integration tests of `bee-deps`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-deps --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "parsers.rs"]
mod parsers;
#[path = "sbom.rs"]
mod sbom;
#[path = "security.rs"]
mod security;
