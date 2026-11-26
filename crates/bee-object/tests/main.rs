//! The integration tests of `bee-object`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-object --test integration <file_stem>::`.

#[path = "fuzz.rs"]
mod fuzz;
#[path = "vectors.rs"]
mod vectors;
