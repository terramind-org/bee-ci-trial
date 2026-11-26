//! The integration tests of `bee-mcp`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-mcp --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "e2e.rs"]
mod e2e;
#[path = "protocol.rs"]
mod protocol;
#[path = "schemas.rs"]
mod schemas;
