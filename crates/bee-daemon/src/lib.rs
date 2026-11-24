//! Cache tiers of bee (SPEC §10.2, §6.5, §10.5; PRODUCT §4.5, §4.6; `docs/spec/cache-tiers.md`).
//!
//! One binary, two roles:
//!
//! - **Node daemon** (`bee-daemon serve`): one per machine, shared by every working copy,
//!   agent and CI job on it over a unix socket. A content-addressed disk cache
//!   ([`disk::DiskCache`]: BLAKE3-verified, size-bounded LRU with pinning, atomic writes, a
//!   crash-safe index, one copy per object across repos), fetch coalescing
//!   ([`coalesce::Coalescer`]), prefetch by sparse profile, signed-URL CDN fetches with API
//!   fallback, snapshots that materialize a revision by cloning or hard-linking cache files
//!   ([`snapshot`]), and a file-system watcher journal for `status` ([`watch`]).
//! - **Regional cache** (`bee-daemon regional`): the same tier over TCP with an in-memory
//!   store ([`memory::MemoryCache`]), in front of the server for a region.
//!
//! Both answer `bee.v1.ObjectService` exactly as the server does ([`service`]), forward every
//! other RPC byte for byte with the caller's credential, and never serve an object to a caller
//! the tier above has not authorized ([`tier`]). Clients reach a tier through
//! [`client::BlockingClient`], whose `unary` / `server_stream` / `client_stream` methods have
//! the shape of `bee_wc::Transport`, so a working copy can run through the daemon unchanged.

pub mod bootstrap;
pub mod client;
pub mod coalesce;
pub mod config;
pub mod disk;
pub mod error;
mod lru;
pub mod memory;
pub mod server;
pub mod service;
pub mod snapshot;
pub mod store;
pub mod tier;
pub mod upstream;
pub mod watch;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use client::{BlockingClient, ConnectClient, Endpoint};
pub use disk::{DiskCache, DiskConfig};
pub use memory::MemoryCache;
pub use server::Running;
pub use store::ObjectStore;
pub use tier::{Tier, TierConfig, TierKind};
pub use upstream::{Auth, HttpUpstream, Upstream};

/// The socket a daemon listens on by default: `$BEE_DAEMON_SOCKET`, else
/// `$XDG_RUNTIME_DIR/bee/daemon.sock`, else `$HOME/.bee/daemon.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("BEE_DAEMON_SOCKET") {
        return PathBuf::from(p);
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("bee").join("daemon.sock");
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    home.join(".bee").join("daemon.sock")
}

/// The cache directory used by default: `$BEE_DAEMON_CACHE`, else `$HOME/.bee/cache`.
pub fn default_cache_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("BEE_DAEMON_CACHE") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    home.join(".bee").join("cache")
}

/// A node-daemon tier: a disk cache filled from `upstream_url`.
pub fn daemon_tier(
    disk: DiskConfig,
    upstream_url: &str,
    cfg: TierConfig,
) -> error::Result<Arc<Tier>> {
    let store: Arc<dyn ObjectStore> = Arc::new(DiskCache::open(disk)?);
    let upstream: Arc<dyn Upstream> =
        Arc::new(HttpUpstream::new(upstream_url, Duration::from_secs(300))?);
    Ok(Arc::new(Tier::new(cfg, store, upstream)))
}

/// A regional-cache tier: `memory_bytes` of objects in memory, filled from `upstream_url`.
pub fn regional_tier(
    memory_bytes: u64,
    upstream_url: &str,
    cfg: TierConfig,
) -> error::Result<Arc<Tier>> {
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryCache::new(memory_bytes, 90));
    let upstream: Arc<dyn Upstream> =
        Arc::new(HttpUpstream::new(upstream_url, Duration::from_secs(300))?);
    Ok(Arc::new(Tier::new(cfg, store, upstream)))
}
