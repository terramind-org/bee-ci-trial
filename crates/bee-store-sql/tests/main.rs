//! The integration tests of `bee-store-sql`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-store-sql --test integration <file_stem>::`.

#[macro_use]
mod common;

#[path = "bench_hot_ref.rs"]
mod bench_hot_ref;
#[path = "complexity.rs"]
mod complexity;
#[path = "concurrency.rs"]
mod concurrency;
#[path = "faults.rs"]
mod faults;
#[path = "gc.rs"]
mod gc;
#[path = "model.rs"]
mod model;
#[path = "sql.rs"]
mod sql;
#[path = "store_semantics.rs"]
mod store_semantics;
