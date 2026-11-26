//! The integration tests of `bee-proto`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-proto --test integration <file_stem>::`.

#[path = "error_contract.rs"]
mod error_contract;
#[path = "gateway.rs"]
mod gateway;
#[path = "generated_files.rs"]
mod generated_files;
#[path = "wire_contract.rs"]
mod wire_contract;
