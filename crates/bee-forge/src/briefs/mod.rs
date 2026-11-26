//! Change requests as briefs: evidence (screenshots, before and after, video, benchmarks,
//! image sets, audio, documents, preview links, test reports) and the advisory checklist of
//! what a CR still owes (SPEC §11.9; the rules are in `docs/spec/forge-briefs.md`). Serves
//! `BriefService`. The brief itself lives on the CR ([`crate::changes::brief`]).
//!
//! Nothing here is consulted by landing: the checklist nudges, it never gates.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | `mod.rs` | the surface, table, migration, event kinds, audit prefix and bounds |
//! | [`model`] | the stored record |
//! | [`rules`] | pure checks of an evidence spec and its assets: per-kind roles, kinds, alt text, sizes |
//! | [`checklist`] | the checklist, a pure function of a revision's path classes, the brief and the evidence |
//! | [`store`] | [`Briefs`]: adding (inline, from a workflow artifact, without files), uploads, finalize, reads, updates, deletes |
//! | [`transfer`] | the signed `/_evidence/...` upload and download URLs and what they serve |
//! | [`maintain`] | upkeep: pending uploads not finalized within [`PENDING_TTL_MS`] are deleted |
//! | [`service`] | the tonic `BriefService` |
//!
//! # Storage
//!
//! One table on its own `briefs` surface; bytes live in the CI blob store
//! ([`crate::ci::ports::CiBlobs`], in the repo's org namespace) and their URLs are signed with
//! [`crate::ci::ports::UrlSigner`]. Byte checks reuse [`crate::media`].
//!
//! | table | scope | id | keys |
//! |---|---|---|---|
//! | evidence | repo | evidence | `k1` change request, `n1` expiry ms (pending only) |

pub mod checklist;
pub mod maintain;
pub mod model;
pub mod rules;
pub mod service;
pub mod store;
pub mod transfer;

pub use store::{ArtifactSource, BriefPorts, Briefs};

use crate::module::{EventKind, ForgeModule};
use crate::store::{Index, Migration, MigrationOp, Phase, Surface, TableDef};

/// The briefs surface.
pub const SURFACE: Surface = Surface("briefs");

/// A pending item not finalized within this long is deleted.
pub const PENDING_TTL_MS: u64 = 60 * 60 * 1000;
/// How long a signed upload URL is valid.
pub const UPLOAD_URL_TTL_MS: u64 = 15 * 60 * 1000;
/// How long a signed download URL is valid.
pub const DOWNLOAD_URL_TTL_MS: u64 = crate::media::upload::DOWNLOAD_TTL_MS;
/// Most change requests one `ListEvidence` names.
pub const MAX_LIST_CHANGES: usize = 100;
/// URL path prefix of evidence transfers.
pub const URL_PREFIX: &str = "/_evidence";

/// Evidence items.
pub static EVIDENCE: TableDef = TableDef {
    name: "forge_evidence",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::N1],
};

/// Creates the evidence table.
pub static M1: Migration = Migration {
    module: "briefs",
    version: 1,
    name: "create_evidence",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&EVIDENCE)],
};

/// What the briefs module owns. `cr.brief_set` is the changes module's (the brief is
/// written in its transaction).
pub static MODULE: ForgeModule = ForgeModule {
    name: "briefs",
    surfaces: &[SURFACE],
    migrations: &[&M1],
    event_kinds: &[
        EventKind::new(
            "evidence.added",
            "Evidence was added to a change request (or its upload finalized)",
        ),
        EventKind::new(
            "evidence.updated",
            "Evidence of a change request was edited",
        ),
        EventKind::new(
            "evidence.deleted",
            "Evidence of a change request was deleted",
        ),
    ],
    audit_prefixes: &["evidence."],
};
