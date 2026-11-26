//! Packages and Pages (PRODUCT §4.17). Serves `PackageService` and `PagesService`; the
//! contract is `docs/spec/packages-pages.md`.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | [`model`] | stored records, the [`model::Eco`] ecosystem enum |
//! | [`names`] | per-ecosystem name and version rules, Maven version order |
//! | [`hash`], [`archive`] | SHA-1, MD5, base64; tar, gzip and zip readers |
//! | [`ports`] | what the server backs: the blob store, team membership, provenance checks |
//! | [`manage`] | the store: packages, versions, tags, grants, downloads, retention, native publish |
//! | [`access`] | who may read, write and administer a package |
//! | [`registry`] | the transport-agnostic HTTP front of every registry |
//! | [`oci`], [`npm`], [`maven`], [`nuget`], [`rubygems`], [`cargo`] | the protocol handlers |
//! | [`svc`] | the `PackageService` tonic service |
//!
//! Pages lives in [`crate::pages`] and shares this module's [`MODULE`] declaration (surfaces
//! `packages` and `pages`).
//!
//! # Surfaces
//!
//! `packages` (every package table) and `pages` (sites, deployments, domains), each with its
//! own pool so a registry push storm cannot starve site serving.

pub mod access;
pub mod archive;
pub mod cargo;
pub(crate) mod http;
pub mod manage;
pub mod maven;
pub mod model;
pub mod names;
pub mod npm;
pub mod nuget;
pub mod oci;
pub mod ports;
pub mod registry;
pub mod rubygems;
pub mod svc;

pub mod hash;
mod lifecycle;

use std::sync::Arc;
use std::time::Duration;

use crate::module::{EventKind, ForgeModule};
use crate::store::{Index, Migration, MigrationOp, Phase, Surface, TableDef};

pub use self::lifecycle::UploadFileSpec;
pub use self::manage::{
    ListFilter, NewFile, NewVersion, OnConflict, Owner, Packages, PackagesConfig, UploadTarget,
};
pub use self::ports::{
    BlobPort, DetachedBlobs, MemoryBlobs, PackagePorts, ProvenanceClaim, ProvenanceVerifier,
    TeamMembership,
};
pub use self::registry::PackageRegistry;

/// The packages surface.
pub const SURFACE: Surface = Surface("packages");

/// Packages, scoped by owner id; `k1` live name key (unique), `k2` name key while deleted.
pub static PACKAGES: TableDef = TableDef {
    name: "forge_packages",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeK2, Index::N1],
};
/// Package-to-repo links, scoped by repo id; `k1` owner id.
pub static LINKS: TableDef = TableDef {
    name: "forge_package_links",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Versions, scoped by package id; `k1` live version key (unique), `k2` subject digest or
/// `del:<key>` while deleted, `n1` deletion time.
pub static VERSIONS: TableDef = TableDef {
    name: "forge_package_versions",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeK2, Index::N1],
};
/// Files, scoped by version id.
pub static FILES: TableDef = TableDef {
    name: "forge_package_files",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// OCI blobs, scoped by package id.
pub static BLOBS: TableDef = TableDef {
    name: "forge_package_blobs",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Reference counts of blob store keys, scoped by owner id; `n1` the count.
pub static BLOBREFS: TableDef = TableDef {
    name: "forge_package_blobrefs",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Tags and dist-tags, scoped by package id; `k1` version id.
pub static TAGS: TableDef = TableDef {
    name: "forge_package_tags",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};
/// Access grants, scoped by package id.
pub static GRANTS: TableDef = TableDef {
    name: "forge_package_grants",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Upload sessions, scoped by owner id; `n1` expiry.
pub static UPLOADS: TableDef = TableDef {
    name: "forge_package_uploads",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::N1],
};
/// Download and storage counters, scoped by package id (or owner id for `storage`); `n1`.
pub static COUNTS: TableDef = TableDef {
    name: "forge_package_counts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Daily download counters, scoped by package id; id `<yyyymmdd>:<version id>`, `n1`.
pub static DAILY: TableDef = TableDef {
    name: "forge_package_daily",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Build provenance, scoped by package id; id is the version id.
pub static PROVENANCE: TableDef = TableDef {
    name: "forge_package_provenance",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Owner settings and retention policies, scoped by owner id.
pub static OWNER: TableDef = TableDef {
    name: "forge_package_owner",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

static M1: Migration = Migration {
    module: "packages",
    version: 1,
    name: "create_packages",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&PACKAGES),
        MigrationOp::CreateTable(&LINKS),
        MigrationOp::CreateTable(&VERSIONS),
        MigrationOp::CreateTable(&FILES),
        MigrationOp::CreateTable(&BLOBS),
        MigrationOp::CreateTable(&BLOBREFS),
        MigrationOp::CreateTable(&TAGS),
        MigrationOp::CreateTable(&GRANTS),
        MigrationOp::CreateTable(&UPLOADS),
        MigrationOp::CreateTable(&COUNTS),
        MigrationOp::CreateTable(&DAILY),
        MigrationOp::CreateTable(&PROVENANCE),
        MigrationOp::CreateTable(&OWNER),
    ],
};

static M2: Migration = Migration {
    module: "packages",
    version: 2,
    name: "create_pages",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&crate::pages::SITES),
        MigrationOp::CreateTable(&crate::pages::DEPLOYMENTS),
        MigrationOp::CreateTable(&crate::pages::MANIFESTS),
        MigrationOp::CreateTable(&crate::pages::DOMAINS),
        MigrationOp::CreateTable(&crate::pages::HOSTS),
        MigrationOp::CreateTable(&crate::pages::VERIFICATIONS),
        MigrationOp::CreateTable(&crate::pages::FILEREFS),
        MigrationOp::CreateTable(&crate::pages::OWNERS),
    ],
};

/// Package event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::gh(
        "package.published",
        "A package version was published",
        "package",
        "published",
    ),
    EventKind::gh(
        "package.deleted",
        "A package was deleted",
        "package",
        "deleted",
    ),
    EventKind::new("package.created", "A package was created"),
    EventKind::new("package.updated", "A package was updated"),
    EventKind::new("package.restored", "A deleted package was restored"),
    EventKind::new(
        "package.visibility_changed",
        "A package's visibility changed",
    ),
    EventKind::new("package.linked", "A package was linked to a repo"),
    EventKind::new("package.unlinked", "A package's repo link was removed"),
    EventKind::new("package.access_granted", "Access to a package was granted"),
    EventKind::new("package.access_revoked", "Access to a package was revoked"),
    EventKind::new("package.version_deleted", "A package version was deleted"),
    EventKind::new("package.version_restored", "A package version was restored"),
    EventKind::new(
        "package.version_updated",
        "A package version was deprecated, yanked or unyanked",
    ),
    EventKind::new("package.tags_updated", "A package version's tags changed"),
    EventKind::new(
        "package.retention_applied",
        "A retention policy removed package versions",
    ),
];

const PARTS: [&[EventKind]; 2] = [EVENTS, crate::pages::EVENTS];

const KIND_COUNT: usize = EVENTS.len() + crate::pages::EVENTS.len();

const KINDS: [EventKind; KIND_COUNT] = {
    let mut out = [EventKind::new("", ""); KIND_COUNT];
    let (mut n, mut p) = (0, 0);
    while p < PARTS.len() {
        let mut i = 0;
        while i < PARTS[p].len() {
            out[n] = PARTS[p][i];
            n += 1;
            i += 1;
        }
        p += 1;
    }
    out
};

/// What the packages module owns (packages and Pages).
pub static MODULE: ForgeModule = ForgeModule {
    name: "packages",
    surfaces: &[SURFACE, crate::pages::SURFACE],
    migrations: &[&M1, &M2],
    event_kinds: &KINDS,
    audit_prefixes: &["package.", "pages."],
};

/// Where each registry is mounted on the server, relative to the host root. The OCI
/// distribution API must be at `/v2/` by specification; the others are free but fixed here so
/// generated URLs (npm tarballs, NuGet resources, Cargo `config.json`) agree with the mount.
pub mod mounts {
    /// OCI distribution (`/v2/...`).
    pub const OCI: &str = "/v2/";
    /// npm (`/npm/<owner>/...`).
    pub const NPM: &str = "/npm/";
    /// Maven (`/maven/<owner>/<group path>/<artifact>/<version>/<file>`).
    pub const MAVEN: &str = "/maven/";
    /// NuGet v3 (`/nuget/<owner>/v3/index.json`).
    pub const NUGET: &str = "/nuget/";
    /// RubyGems (`/rubygems/<owner>/...`).
    pub const RUBYGEMS: &str = "/rubygems/";
    /// Cargo sparse registry (`/cargo/<owner>/index/...`, `/cargo/<owner>/api/v1/...`).
    pub const CARGO: &str = "/cargo/";
}

/// Default window in which deleted packages and versions can be restored.
pub const RESTORE_WINDOW: Duration = Duration::from_secs(30 * 24 * 3600);

/// A shared handle on the blob port, used by Pages too.
pub type Blobs = Arc<dyn BlobPort>;
