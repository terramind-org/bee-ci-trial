//! Faithful in-memory implementations of both stores (SPEC §14.3), always compiled: tests,
//! dev tools and the reference for the SQL and S3 implementations all use them.

mod blob;
mod meta;

pub use blob::MemoryBlobStore;
pub use meta::MemoryMetaStore;
