//! Beehive: orgs, repos, change requests, reviews, checks, leases, events, provenance.
//!
//! The contract is SPEC.md §11 (forge), §12.2 (errors) and §15.1 (failure domains); the
//! conventions every forge module follows are in `docs/spec/forge-core.md`.
//!
//! # Layout
//!
//! - [`store`]: the [`store::ForgeStore`] row store (memory, SQLite, Postgres) with
//!   per-surface pools, expand/contract migrations and outboxes. Forge data never lives in
//!   the VCS `MetaStore`.
//! - [`module`]: every area declares its surfaces, migrations, event kinds and audit prefixes;
//!   [`module::Registry`] composes them.
//! - Core areas: [`orgs`] (orgs, members, repos and repo settings), [`events`] (event logs,
//!   subscriptions, webhooks), [`leases`], [`checks`], [`audit`] (hash-chained audit log).
//! - [`outbox`]: moves events and audit entries out of each surface's outbox.
//! - [`auth`]: token verification and the authorization pattern every service uses.
//! - [`service`]: tonic implementations of `OrgService`, `RepoService`, `LeaseService`,
//!   `CheckService`, `EventService` and `AuditService`.
//! - [`forge`]: wires everything together, with bounded background workers.
//! - Extension points for later workers: [`changes`], [`briefs`] (evidence and the brief
//!   checklist of change requests), [`work`], [`community`],
//!   [`enterprise`], [`ci`], [`packages`], [`pages`], [`security`], [`agents`], [`bisect`]
//!   (post-land bisect and automatic revert over [`ci`] and [`changes`]), [`forks`] (fork
//!   networks sharing one object namespace, sync, cross-fork change requests).

pub mod agents;
pub mod audit;
pub mod auth;
pub mod bisect;
pub mod briefs;
pub mod changes;
pub mod checks;
pub mod ci;
pub mod community;
pub mod cx;
pub mod enterprise;
pub mod error;
pub mod events;
pub mod forge;
pub mod forks;
pub mod hooks;
pub mod ids;
pub mod leases;
pub mod markdown;
pub mod media;
pub mod module;
pub mod orgs;
pub mod outbox;
pub mod packages;
pub mod pages;
pub mod posts;
/// Asked-against-done records and SafetyService.
pub mod safety;
pub mod security;
pub mod service;
pub mod store;
pub mod swarms;
pub mod work;

pub use cx::Cx;
pub use error::{ForgeError, Result};
pub use forge::{Forge, ForgeConfig};

/// Mints bearer tokens with the currently active signing key.
///
/// Servers with a shared key ring implement this by reading their refreshed ring for every
/// mint, while single-process users may use a [`bee_auth::KeyRing`] directly.
pub trait TokenIssuer: Send + Sync {
    /// Mints `request` at `now` seconds since the epoch.
    fn mint(&self, request: &bee_auth::MintRequest, now: u64) -> Result<bee_auth::Token>;
}

impl TokenIssuer for bee_auth::KeyRing {
    fn mint(&self, request: &bee_auth::MintRequest, now: u64) -> Result<bee_auth::Token> {
        self.mint(request, now).map_err(ForgeError::from)
    }
}
