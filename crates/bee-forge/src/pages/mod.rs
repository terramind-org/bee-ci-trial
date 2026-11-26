//! Beehive Pages (PRODUCT §4.17): a static site per repo, served from a ref and folder or
//! from a workflow artifact. Serves `PagesService`; the contract is
//! `docs/spec/packages-pages.md`.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | [`model`] | stored records |
//! | [`ports`] | VCS (the community port), blob store, DNS, ACME, artifacts, environment gate |
//! | [`site`] | the site record: create, update, delete, URLs |
//! | [`deploy`] | builds, deployments, content-addressed manifests, atomic switch, previews |
//! | [`domains`] | custom domains, DNS health, certificates, org domain verification |
//! | [`serve`] | the pure serving function and the host lookup around it |
//! | [`listener`] | event reactions: rebuild on push, preview per change request |
//! | [`svc`] | the `PagesService` tonic service |
//!
//! # Deployments
//!
//! A deployment is a content-addressed manifest (path to SHA-256 blob id, size, content type,
//! plus the parsed `_redirects` rules) whose files live in the blob store under
//! `pages/<repo id>/<sha256>`. A site serves `current_deployment_id`; switching it is one row
//! update in one transaction, so a request sees the old site or the new one, never a mix.
//! Identical files and identical manifests are stored once (reference counted).

pub mod deploy;
pub mod domains;
pub mod listener;
pub mod model;
pub mod ports;
pub mod serve;
pub mod site;
pub mod svc;

use std::sync::Arc;

use crate::auth::Authz;
use crate::community::ports::ContentVcs;
use crate::module::EventKind;
use crate::orgs::Orgs;
use crate::packages::BlobPort;
use crate::store::{Db, Index, Surface, TableDef};

pub use self::deploy::DeployOptions;
pub use self::ports::{
    AcmeIssuer, Artifact, ArtifactSource, DeployGate, DnsAnswer, DnsResolver, IssuedCert,
    MemoryAcme, MemoryArtifacts, MemoryDns, OpenGate, PagesPorts,
};
pub use self::serve::{ServeRequest, Served, SiteView};

/// The pages surface.
pub const SURFACE: Surface = Surface("pages");

/// Sites, scoped by org id; id is the repo id.
pub static SITES: TableDef = TableDef {
    name: "forge_pages_sites",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Deployments, scoped by repo id; `k1` kind (`live` or `preview`), `n1` expiry of a preview.
pub static DEPLOYMENTS: TableDef = TableDef {
    name: "forge_pages_deployments",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::N1],
};
/// Manifests, scoped by repo id; id is the manifest id, `n1` the count of deployments using it.
pub static MANIFESTS: TableDef = TableDef {
    name: "forge_pages_manifests",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Reserved: per-site domain rows (the claim itself lives in [`HOSTS`]).
pub static DOMAINS: TableDef = TableDef {
    name: "forge_pages_domains",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Host names that serve a site (custom domains, preview hosts); global scope, id is the host.
pub static HOSTS: TableDef = TableDef {
    name: "forge_pages_hosts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};
/// Org domain verifications, scoped by org id; `k1` the domain.
pub static VERIFICATIONS: TableDef = TableDef {
    name: "forge_pages_verifications",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::K1],
};
/// Reference counts of site content blobs, scoped by repo id; id is the SHA-256 hex, `n1`.
pub static FILEREFS: TableDef = TableDef {
    name: "forge_pages_filerefs",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Per-org Pages settings, scoped by org id.
pub static OWNERS: TableDef = TableDef {
    name: "forge_pages_owners",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Pages event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::gh(
        "page_build.completed",
        "A Pages build finished",
        "page_build",
        "",
    ),
    EventKind::new("pages.site_created", "A Pages site was created"),
    EventKind::new("pages.site_updated", "A Pages site was updated"),
    EventKind::new("pages.site_deleted", "A Pages site was deleted"),
    EventKind::new("pages.deployment_succeeded", "A Pages deployment went live"),
    EventKind::new("pages.deployment_failed", "A Pages deployment failed"),
    EventKind::new(
        "pages.deployment_cancelled",
        "A Pages deployment was cancelled",
    ),
    EventKind::new(
        "pages.preview_created",
        "A Pages preview deployment was created",
    ),
    EventKind::new(
        "pages.preview_removed",
        "A Pages preview deployment was removed",
    ),
    EventKind::new(
        "pages.domain_set",
        "A Pages custom domain was set or cleared",
    ),
    EventKind::new(
        "pages.domain_verified",
        "An org domain was verified for Pages",
    ),
    EventKind::new("pages.certificate_issued", "A Pages certificate was issued"),
    EventKind::new(
        "pages.certificate_failed",
        "A Pages certificate request failed",
    ),
];

/// Settings of Pages.
#[derive(Clone, Debug)]
pub struct PagesConfig {
    /// Domain under which sites are served: `https://<org>.<pages_domain>/<repo>/`.
    pub pages_domain: String,
    /// Addresses of the Pages servers (A records a custom domain may point at).
    pub pages_ips: Vec<String>,
    /// The CA a CAA record must allow for certificates to be issued.
    pub acme_caa_issuer: String,
    /// Most files in a site.
    pub max_files: usize,
    /// Largest site, total bytes.
    pub max_site_bytes: u64,
    /// Largest single file.
    pub max_file_bytes: u64,
    /// Days a preview deployment lives unless the site says otherwise.
    pub preview_ttl_days: u32,
    /// Finished deployments kept per site before the oldest are pruned.
    pub keep_deployments: usize,
    /// Certificates are renewed this many days before they expire.
    pub renew_before_days: u32,
}

impl Default for PagesConfig {
    fn default() -> PagesConfig {
        PagesConfig {
            pages_domain: "pages.beehive.invalid".into(),
            pages_ips: vec!["192.0.2.10".into(), "192.0.2.11".into()],
            acme_caa_issuer: "letsencrypt.org".into(),
            max_files: 20_000,
            max_site_bytes: 1 << 30,
            max_file_bytes: 100 << 20,
            preview_ttl_days: 14,
            keep_deployments: 10,
            renew_before_days: 30,
        }
    }
}

/// Pages over the forge store.
pub struct Pages {
    pub(crate) db: Db,
    pub(crate) orgs: Arc<Orgs>,
    pub(crate) authz: Authz,
    pub(crate) vcs: Arc<dyn ContentVcs>,
    pub(crate) blobs: Arc<dyn BlobPort>,
    pub(crate) ports: PagesPorts,
    pub(crate) config: PagesConfig,
}

impl Pages {
    /// Pages over `db`; `vcs` and `blobs` are the defaults for ports left unset.
    pub fn new(
        db: Db,
        orgs: Arc<Orgs>,
        authz: Authz,
        vcs: Arc<dyn ContentVcs>,
        blobs: Arc<dyn BlobPort>,
        ports: PagesPorts,
        config: PagesConfig,
    ) -> Pages {
        Pages {
            db,
            orgs,
            authz,
            vcs: ports.vcs.clone().unwrap_or(vcs),
            blobs: ports.blobs.clone().unwrap_or(blobs),
            ports,
            config,
        }
    }

    /// Settings.
    pub fn config(&self) -> &PagesConfig {
        &self.config
    }

    pub(crate) fn blob_key(repo_id: &str, blob: &str) -> String {
        format!("pages/{repo_id}/{blob}")
    }

    /// One maintenance pass: expired previews, certificate renewal and expiry.
    pub async fn maintain(&self, cx: &crate::cx::Cx) -> crate::error::Result<()> {
        self.expire_previews(cx).await?;
        self.renew_certificates(cx).await?;
        Ok(())
    }
}
