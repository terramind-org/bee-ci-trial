//! The integration tests of `bee-workflow`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-workflow --test integration <file_stem>::`.

#[path = "golden.rs"]
mod golden;
#[path = "properties.rs"]
mod properties;
