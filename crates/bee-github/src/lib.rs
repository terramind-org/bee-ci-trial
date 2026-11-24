//! GitHub migration for bee and Beehive (SPEC §17, PLAN task W17): one command from a
//! GitHub repository to Beehive with identical commit ids, preserved PR and issue numbers,
//! claimable placeholder identities, live sync until cutover, and a fidelity report that
//! explains every dropped item.
//!
//! # Layout
//!
//! | module | owns |
//! |---|---|
//! | [`auth`], [`client`], [`clock`] | credentials (token, GitHub App, `gh auth token`), the rate-limit-aware REST/GraphQL client |
//! | [`model`] | the GitHub objects read, and [`model::Metadata`] |
//! | [`state`] | the resumable state directory: checkpoint and journal |
//! | [`fetch`], [`archive`] | API backfill in checkpointed steps; the org migration archive |
//! | [`git`], [`target`] | the bare mirror fetched with system git; where history goes ([`target::GitTarget`]) |
//! | [`principals`] | placeholder principals and claims |
//! | [`convert`], [`links`] | GitHub records → sink records; link rewriting and the redirect map |
//! | [`policy`] | branch protection and CODEOWNERS → `.beehive/policy.toml` |
//! | [`sink`], [`hub`] | [`sink::ForgeSink`], the in-memory sink, and the Beehive forge adapter |
//! | [`import`] | the [`import::Importer`]: phases, resume, claims, dry run |
//! | [`live`] | webhook receiver, polling fallback, cutover |
//! | [`server`] | importing through a running `bee-server` |
//! | [`report`] | the fidelity report (`schemas/v1/import-report.schema.json`) |
//!
//! The design and the GitHub → Beehive mapping are in `docs/spec/importers.md`.

pub mod archive;
pub mod auth;
pub mod client;
pub mod clock;
pub mod convert;
pub mod error;
pub mod fetch;
pub mod git;
pub mod hub;
pub mod import;
pub mod links;
pub mod live;
pub mod model;
pub mod policy;
pub mod principals;
pub mod report;
pub mod server;
pub mod sink;
pub mod state;
pub mod target;

pub use client::{Client, ClientConfig};
pub use import::{Importer, Options, RunResult};
pub use report::FidelityReport;
pub use sink::{ForgeSink, MemorySink};
