//! Git smart HTTP (upload-pack, receive-pack) and Git LFS served from bee, with pushes
//! landing under policy (SPEC §17.4, `docs/spec/git-gateway.md`).
//!
//! The server mounts [`GitGateway::router`] on its single listener and implements
//! [`GitBackend`] over its state (tokens, policy, push protection, audit, the Land coalescer).
//! This crate does everything git-specific:
//!
//! - [`pktline`]: pkt-line framing and side-band.
//! - [`refs`]: git ↔ bee ref names, per principal.
//! - [`gitmap`]: git ids and bytes of bee objects with the exact SPEC §17.4 rules, cached as
//!   `git_mapping` rows.
//! - [`advertise`]: the refs a principal sees, peeled tags and `HEAD`.
//! - [`walk`]: what a fetch sends (negotiation, shallow, filters) and the pack.
//! - [`cache`]: the pack cache keyed by the request digest.
//! - [`upload`]: `git-upload-pack`, protocol v2 and v0/v1.
//! - [`ingest`]: pushed commits → changesets with their exact git ids.
//! - [`receive`]: `git-receive-pack`: Land for protected refs, CAS for the rest, report-status.
//! - [`lfs`]: the LFS batch API with signed transfer URLs.
//! - [`http`]: the routes and authentication.
//! - [`metrics`]: the gateway's Prometheus counters.

pub mod advertise;
pub mod backend;
pub mod cache;
mod error;
pub mod gitmap;
pub mod http;
pub mod ingest;
pub mod lfs;
pub mod locks;
pub mod metrics;
pub mod pktline;
pub mod receive;
pub mod refs;
pub mod ssh;
pub mod upload;
pub mod walk;

pub use backend::{
    Access, GitBackend, LfsCredential, OpenRequest, PushContext, RefState, RepoSession, Via,
};
pub use error::{GitError, GitResult};
pub use http::{GatewayConfig, GitGateway, credential};
