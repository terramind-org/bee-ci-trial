//! Interfaces the forge needs from the rest of the system, with the implementations that
//! live here.
//!
//! - [`PrincipalDirectory`]: who a principal id or login is (the user service owns this;
//!   [`MemoryDirectory`] learns principals from verified tokens and registrations).
//! - [`RepoProvisioner`]: creates the VCS side of a new org or repo before the forge row
//!   exists ([`MetaProvisioner`] over a [`bee_store::MetaStore`]; [`DetachedProvisioner`]
//!   for forge-only deployments where the VCS side is provisioned elsewhere).
//! - [`RevisionResolver`]: turns a revision expression into a changeset id
//!   ([`HexResolver`] accepts full hex ids; the server installs a VCS-backed one).
//! - [`PolicySource`]: the repo's effective `bee_policy::Policy`, when available.
//!
//! The dependency direction is forge → VCS only; nothing here is called by the VCS data path.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_core::Ulid;
use bee_proto::v1::{ObjectId, Org, Principal, Repo};
use bee_store::{MetaStore, OrgId, OrgRow, RepoId, RepoRow};

use crate::error::{ForgeError, Result};
use crate::ids::{Clock, unhex};

/// Looks up principals.
#[async_trait]
pub trait PrincipalDirectory: Send + Sync {
    /// A principal by id.
    async fn get(&self, id: &str) -> Option<Principal>;
    /// A principal by login.
    async fn by_login(&self, login: &str) -> Option<Principal>;
    /// Logins and verified emails of a principal (for invitation matching).
    async fn identities(&self, id: &str) -> (Vec<String>, Vec<String>);
    /// Records a principal seen with a verified token.
    fn observe(&self, p: &Principal);
}

#[derive(Clone, Default)]
struct Entry {
    principal: Principal,
    logins: Vec<String>,
    emails: Vec<String>,
}

/// An in-memory directory.
#[derive(Default)]
pub struct MemoryDirectory {
    map: RwLock<BTreeMap<String, Entry>>,
}

impl MemoryDirectory {
    /// An empty directory.
    pub fn new() -> MemoryDirectory {
        MemoryDirectory::default()
    }

    /// Registers a principal with logins and emails.
    pub fn register(&self, p: Principal, logins: Vec<String>, emails: Vec<String>) {
        if let Ok(mut m) = self.map.write() {
            m.insert(
                p.id.clone(),
                Entry {
                    principal: p,
                    logins,
                    emails,
                },
            );
        }
    }
}

#[async_trait]
impl PrincipalDirectory for MemoryDirectory {
    async fn get(&self, id: &str) -> Option<Principal> {
        self.map.read().ok()?.get(id).map(|e| e.principal.clone())
    }

    async fn by_login(&self, login: &str) -> Option<Principal> {
        self.map
            .read()
            .ok()?
            .values()
            .find(|e| e.logins.iter().any(|l| l.eq_ignore_ascii_case(login)))
            .map(|e| e.principal.clone())
    }

    async fn identities(&self, id: &str) -> (Vec<String>, Vec<String>) {
        self.map
            .read()
            .ok()
            .and_then(|m| m.get(id).map(|e| (e.logins.clone(), e.emails.clone())))
            .unwrap_or_default()
    }

    fn observe(&self, p: &Principal) {
        if let Ok(mut m) = self.map.write() {
            m.entry(p.id.clone()).or_insert_with(|| Entry {
                principal: p.clone(),
                ..Entry::default()
            });
        }
    }
}

/// Options of a new repo's initial content.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoInit {
    /// Add a README in the root changeset.
    pub auto_init: bool,
    /// SPDX id of a license file to add.
    pub license_template: String,
}

/// Creates the VCS side of orgs and repos. Calls are idempotent by id.
#[async_trait]
pub trait RepoProvisioner: Send + Sync {
    /// Creates the org's VCS row.
    async fn create_org(&self, org: &Org) -> Result<()>;
    /// Creates the repo's VCS row (and initial content when asked).
    async fn create_repo(&self, repo: &Repo, init: &RepoInit) -> Result<()>;
    /// Creates a repo whose content starts from a template's tip.
    async fn create_from_template(
        &self,
        repo: &Repo,
        template: &Repo,
        include_all_refs: bool,
    ) -> Result<()>;
}

/// Provisions only forge rows; the VCS side is provisioned by another component.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedProvisioner;

#[async_trait]
impl RepoProvisioner for DetachedProvisioner {
    async fn create_org(&self, _org: &Org) -> Result<()> {
        Ok(())
    }
    async fn create_repo(&self, _repo: &Repo, _init: &RepoInit) -> Result<()> {
        Ok(())
    }
    async fn create_from_template(&self, _repo: &Repo, _template: &Repo, _all: bool) -> Result<()> {
        Ok(())
    }
}

/// Provisions org and repo rows in the VCS metadata store. Initial content (README,
/// license, template copy) needs the VCS writer and blob store, which the server's
/// provisioner wraps around this one; asking this one for content is `UNIMPLEMENTED`.
pub struct MetaProvisioner {
    meta: Arc<dyn MetaStore>,
    clock: Arc<dyn Clock>,
}

impl MetaProvisioner {
    /// A provisioner over `meta`.
    pub fn new(meta: Arc<dyn MetaStore>, clock: Arc<dyn Clock>) -> MetaProvisioner {
        MetaProvisioner { meta, clock }
    }
}

fn ulid(id: &str) -> Result<Ulid> {
    Ulid::parse(id).map_err(|e| ForgeError::invalid("invalid_id", e.message))
}

fn vcs_err(e: bee_core::BeeError) -> ForgeError {
    match bee_proto::errors::parse_reason(e.reason.as_str()) {
        Some(r) => ForgeError::new(r, format!("vcs store: {}", e.message)),
        None => ForgeError::unavailable(format!("vcs store: {}", e.message)),
    }
}

#[async_trait]
impl RepoProvisioner for MetaProvisioner {
    async fn create_org(&self, org: &Org) -> Result<()> {
        self.meta
            .create_org(OrgRow {
                id: OrgId(ulid(&org.id)?),
                name: org.name.clone(),
                created_at_ms: self.clock.now_ms(),
            })
            .await
            .map(|_| ())
            .map_err(vcs_err)
    }

    async fn create_repo(&self, repo: &Repo, init: &RepoInit) -> Result<()> {
        if init.auto_init || !init.license_template.is_empty() {
            return Err(ForgeError::unimplemented(
                "initial content needs the VCS writer; this deployment provisions rows only",
            ));
        }
        self.meta
            .create_repo(RepoRow {
                id: RepoId(ulid(&repo.id)?),
                org: OrgId(ulid(&repo.org_id)?),
                name: repo.name.clone(),
                created_at_ms: self.clock.now_ms(),
                draft_retention_days: 30,
            })
            .await
            .map(|_| ())
            .map_err(vcs_err)
    }

    async fn create_from_template(&self, _repo: &Repo, _template: &Repo, _all: bool) -> Result<()> {
        Err(ForgeError::unimplemented(
            "copying template content needs the VCS writer; this deployment provisions rows only",
        ))
    }
}

/// Resolves revision expressions to changeset ids.
#[async_trait]
pub trait RevisionResolver: Send + Sync {
    /// The changeset `expr` names in `repo`.
    async fn resolve(&self, repo: &Repo, expr: &str) -> Result<ObjectId>;
}

/// Accepts only full changeset ids in hex.
#[derive(Clone, Copy, Debug, Default)]
pub struct HexResolver;

#[async_trait]
impl RevisionResolver for HexResolver {
    async fn resolve(&self, _repo: &Repo, expr: &str) -> Result<ObjectId> {
        match unhex(expr) {
            Some(raw) if !raw.is_empty() && raw.len() <= 64 => Ok(ObjectId { raw }),
            _ => Err(ForgeError::invalid(
                "unsupported_revision",
                "this server resolves only full hex changeset ids here",
            )),
        }
    }
}

/// Supplies a repo's effective policy.
#[async_trait]
pub trait PolicySource: Send + Sync {
    /// The policy, or `None` when the repo has none (built-in roles decide alone).
    async fn policy(&self, repo: &Repo) -> Result<Option<bee_policy::Policy>>;
}

/// The kind of write a [`WriteGuard`] is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardedWrite {
    /// A new issue.
    Issue,
    /// A comment on an issue or change request.
    Comment,
    /// A new change request.
    ChangeRequest,
}

/// Moderation enforcement at the write paths of issues and change requests: blocks,
/// interaction limits, locks and flood limits. The enterprise module's `Moderation`
/// implements it; install it with [`crate::Forge::set_write_guard`].
#[async_trait]
pub trait WriteGuard: Send + Sync {
    /// Fails with `BLOCKED`, `INTERACTION_LIMITED`, `LOCKED`, `RESOURCE_EXHAUSTED` or another
    /// moderation reason when the write must be refused. `subject` names the conversation
    /// (issue or change request) for comments.
    async fn check(
        &self,
        cx: &crate::cx::Cx,
        repo: &Repo,
        kind: GuardedWrite,
        subject: Option<bee_proto::v1::SubjectRef>,
    ) -> Result<()>;
}
