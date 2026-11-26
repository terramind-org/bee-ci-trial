//! bee-server: the single server binary (VCS core today; Beehive forge areas plug in).
//!
//! One listener serves gRPC (tonic) and the Connect HTTP/JSON gateway from the same service
//! objects, plus `/mcp`, `/healthz`, `/readyz`, `/metrics` and signed object URLs. See
//! `docs/spec/server.md` for configuration, endpoints, metrics and the land coalescer.
//!
//! - [`server::start`] opens the stores and serves; [`server::bootstrap_dev`] creates the
//!   dev org, repo, user and token.
//! - [`services`] holds the VCS services and the [`services::ServiceRegistry`] forge areas
//!   register on.
//! - [`coalescer`] is the required land coalescer (SPEC §7.4).
//! - [`state::AppState`] carries the stores and the authorization helpers every RPC uses.

pub mod access;
pub mod admission;
pub mod audit;
pub mod auth;
pub mod backup;
pub mod coalescer;
pub mod config;
pub mod convert;
pub mod errors;
pub mod events;
pub mod forge;
pub mod git_mount;
pub mod git_ssh;
pub mod integrity;
pub mod keys;
pub mod kv;
pub mod layer;
pub mod logging;
pub mod login;
pub mod metrics;
pub mod oidc;
pub mod otel;
pub mod outage_cache;
pub mod policy;
pub mod preflight;
pub mod redaction;
pub mod server;
pub mod services;
pub mod signatures;
pub mod source_archive;
pub mod stale;
pub mod state;
pub mod tls;
pub mod util;

pub use config::Config;
pub use server::{RunningServer, ServerOptions, start};
