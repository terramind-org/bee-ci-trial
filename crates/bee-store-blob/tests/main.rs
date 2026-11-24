//! The integration tests of `bee-store-blob`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-store-blob --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "disk.rs"]
mod disk;
#[path = "memory.rs"]
mod memory;
#[path = "s3.rs"]
mod s3;
#[path = "s3_stub.rs"]
mod s3_stub;
