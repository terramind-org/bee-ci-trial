//! Cloud workspaces: devcontainer-based environments for humans and agents, as records driven
//! through a [`WorkspaceProvider`].
//!
//! # Lifecycle
//!
//! `CreateWorkspace` resolves the ref, reads and parses the devcontainer file at that
//! changeset (a repo without one gets the default image), picks the machine type (the
//! smallest that meets `hostRequirements` when none is asked for), applies org policy and the
//! owner's workspace quota, then provisions: `PROVISIONING` → `AVAILABLE` (or `FAILED`).
//! `Stop` → `STOPPED` (disk kept, usage recorded, retention clock starts), `Start` →
//! `AVAILABLE` (a pending machine type change applies), `Rebuild` → `REBUILDING` →
//! `AVAILABLE`, `Delete` removes it (refused with unexported changes unless forced).
//! [`Workspaces::maintain`] stops idle workspaces after their idle timeout and deletes stopped
//! ones after their retention period.
//!
//! # Prebuilds
//!
//! Configurations per repo, ref and devcontainer path; triggered on every land of the ref, on
//! lands that change the devcontainer file, on a cron schedule, or manually. A prebuild keeps
//! the newest `retained_versions` ready images; older ones become `SUPERSEDED`.
//!
//! # Egress
//!
//! Policies at org, repo and workspace scope. The effective policy denies by default when any
//! scope does, intersects allow-lists, unions deny-lists. A narrower scope can only restrict:
//! setting one that would allow more than the wider scopes is `POLICY_DENIED`.

use std::sync::Arc;

use bee_proto::v1::{
    ErrorReason, Event, ObjectId, Principal, Provenance, Ref, Repo, SubjectKind, SubjectRef,
    Workspace, WorkspaceConnection, WorkspaceDotfiles, WorkspaceEgressDefault,
    WorkspaceEgressPolicy, WorkspaceEgressScope, WorkspaceMachineType, WorkspacePort,
    WorkspacePortVisibility, WorkspacePrebuild, WorkspacePrebuildConfig,
    WorkspacePrebuildState as Ps, WorkspacePrebuildTrigger, WorkspaceState as Ws, WorkspaceUsage,
    WorkspaceUsageLine, workspace_egress_scope,
};
use sha2::{Digest, Sha256};

use super::devcontainer::{DEFAULT_PATH, DevcontainerSpec};
use super::model::{OrgPolicyRec, PrebuildConfigRec, PrebuildRec, UsageRec, WorkspaceRec};
use super::ports::{AgentRepo, BuildRequest, WorkspaceProvider};
use super::registry::repo_ref_id;
use super::*;
use crate::audit::AuditDraft;
use crate::changes::{Changes, NewChange};
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts, ts_ms};
use crate::store::{Row, Scan, Tx};

/// A machine size offered to workspaces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MachineType {
    /// Name, e.g. `standard-4`.
    pub name: String,
    /// Display name.
    pub display_name: String,
    /// CPUs.
    pub cpus: u32,
    /// Memory.
    pub memory_bytes: u64,
    /// Disk.
    pub storage_bytes: u64,
    /// Has a GPU.
    pub gpu: bool,
    /// Price per hour.
    pub usd_micros_per_hour: u64,
    /// Regions.
    pub regions: Vec<String>,
}

fn mt(
    name: &str,
    display: &str,
    cpus: u32,
    mem_gb: u64,
    disk_gb: u64,
    gpu: bool,
    usd: u64,
) -> MachineType {
    MachineType {
        name: name.into(),
        display_name: display.into(),
        cpus,
        memory_bytes: mem_gb << 30,
        storage_bytes: disk_gb << 30,
        gpu,
        usd_micros_per_hour: usd,
        regions: vec!["local".into()],
    }
}

/// Workspace settings of the deployment.
#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    /// Offered machine types, smallest first.
    pub machine_types: Vec<MachineType>,
    /// Idle timeout when a request sets none.
    pub default_idle_timeout_seconds: u64,
    /// Longest idle timeout.
    pub max_idle_timeout_seconds: u64,
    /// Retention of stopped workspaces when a request sets none.
    pub default_retention_seconds: u64,
    /// Live workspaces per owner when org policy sets none.
    pub max_workspaces_per_owner: u64,
    /// Lifetime of connection tokens.
    pub connection_token_ttl_seconds: u64,
}

impl Default for WorkspaceConfig {
    fn default() -> WorkspaceConfig {
        WorkspaceConfig {
            machine_types: vec![
                mt(
                    "basic-2",
                    "2 cores, 8 GB RAM, 32 GB disk",
                    2,
                    8,
                    32,
                    false,
                    180_000,
                ),
                mt(
                    "standard-4",
                    "4 cores, 16 GB RAM, 32 GB disk",
                    4,
                    16,
                    32,
                    false,
                    360_000,
                ),
                mt(
                    "large-8",
                    "8 cores, 32 GB RAM, 64 GB disk",
                    8,
                    32,
                    64,
                    false,
                    720_000,
                ),
                mt(
                    "xlarge-16",
                    "16 cores, 64 GB RAM, 128 GB disk",
                    16,
                    64,
                    128,
                    false,
                    1_440_000,
                ),
                mt(
                    "gpu-8",
                    "8 cores, 64 GB RAM, 128 GB disk, 1 GPU",
                    8,
                    64,
                    128,
                    true,
                    3_000_000,
                ),
            ],
            default_idle_timeout_seconds: 30 * 60,
            max_idle_timeout_seconds: 4 * 3600,
            default_retention_seconds: 30 * 86_400,
            max_workspaces_per_owner: 10,
            connection_token_ttl_seconds: 3600,
        }
    }
}

/// Org-level workspace policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrgWorkspacePolicy {
    /// Allowed machine types (empty = all).
    pub allowed_machine_types: Vec<String>,
    /// Allowed regions (empty = all).
    pub allowed_regions: Vec<String>,
    /// Public ports allowed.
    pub allow_public_ports: bool,
    /// Longest idle timeout (0 = the deployment's).
    pub max_idle_timeout_seconds: u64,
    /// Live workspaces per owner (0 = the deployment's).
    pub max_workspaces_per_owner: u64,
}

/// A new workspace.
#[derive(Clone, Debug, Default)]
pub struct NewWorkspace {
    /// Name (empty = generated).
    pub name: String,
    /// Ref (empty = default ref).
    pub ref_name: String,
    /// Devcontainer path (empty = `.devcontainer/devcontainer.json`).
    pub devcontainer_path: String,
    /// Machine type (empty = picked).
    pub machine_type: String,
    /// Idle timeout (0 = default).
    pub idle_timeout_seconds: u64,
    /// Retention (0 = default).
    pub retention_seconds: u64,
    /// Region (empty = the machine type's first).
    pub region: String,
    /// Sparse paths.
    pub sparse_paths: Vec<Vec<u8>>,
    /// Start from a ready prebuild when one exists.
    pub use_prebuild: bool,
    /// Same key from the same creator returns the existing workspace.
    pub idempotency_key: String,
    /// The agent run it serves.
    pub agent_run_id: String,
}

fn check_ws_name(n: &str) -> Result<String> {
    let n = n.trim().to_lowercase();
    if n.is_empty()
        || n.len() > 64
        || !n
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || n.starts_with('-')
    {
        return Err(ForgeError::invalid(
            "invalid_name",
            "workspace names use lower-case letters, digits and '-' (at most 64)",
        ));
    }
    Ok(n)
}

/// Whether `pattern` (`example.com` or `*.example.com`) covers `domain` (which may itself be a
/// wildcard).
pub fn domain_covers(pattern: &str, domain: &str) -> bool {
    let (p, d) = (pattern.to_ascii_lowercase(), domain.to_ascii_lowercase());
    if p == d {
        return true;
    }
    match p.strip_prefix("*.") {
        Some(suffix) => {
            let dd = d.strip_prefix("*.").unwrap_or(&d);
            dd.ends_with(&format!(".{suffix}"))
        }
        None => false,
    }
}

fn is_deny(p: &WorkspaceEgressPolicy) -> bool {
    p.default_action == WorkspaceEgressDefault::Deny as i32
}

/// Merges policies from the widest scope to the narrowest into the one in force.
pub fn merge_egress(levels: &[WorkspaceEgressPolicy]) -> WorkspaceEgressPolicy {
    let mut acc = WorkspaceEgressPolicy {
        default_action: WorkspaceEgressDefault::Allow as i32,
        allow_package_registries: true,
        ..Default::default()
    };
    let mut any_allow_list = false;
    let mut any_cidr_list = false;
    for l in levels {
        if is_deny(l) {
            acc.default_action = WorkspaceEgressDefault::Deny as i32;
        }
        if !l.allowed_domains.is_empty() {
            if any_allow_list {
                acc.allowed_domains = l
                    .allowed_domains
                    .iter()
                    .filter(|d| acc.allowed_domains.iter().any(|p| domain_covers(p, d)))
                    .cloned()
                    .collect();
            } else {
                acc.allowed_domains = l.allowed_domains.clone();
                any_allow_list = true;
            }
        }
        if !l.allowed_cidrs.is_empty() {
            if any_cidr_list {
                acc.allowed_cidrs.retain(|c| l.allowed_cidrs.contains(c));
            } else {
                acc.allowed_cidrs = l.allowed_cidrs.clone();
                any_cidr_list = true;
            }
        }
        for d in &l.denied_domains {
            if !acc.denied_domains.contains(d) {
                acc.denied_domains.push(d.clone());
            }
        }
        acc.allow_package_registries &= l.allow_package_registries || !is_deny(l);
        acc.log_denied |= l.log_denied;
    }
    acc.allowed_domains.sort();
    acc.allowed_domains.dedup();
    acc.denied_domains.sort();
    acc
}

/// Why `narrow` would allow more than the wider effective policy `wide`, if it would.
pub fn widens(wide: &WorkspaceEgressPolicy, narrow: &WorkspaceEgressPolicy) -> Option<String> {
    if !is_deny(wide) {
        return None;
    }
    if !is_deny(narrow) {
        return Some("the wider scope denies by default".into());
    }
    for d in &narrow.allowed_domains {
        if !wide.allowed_domains.iter().any(|p| domain_covers(p, d)) {
            return Some(format!("{d} is not allowed by the wider scope"));
        }
    }
    for c in &narrow.allowed_cidrs {
        if !wide.allowed_cidrs.contains(c) {
            return Some(format!("{c} is not allowed by the wider scope"));
        }
    }
    if narrow.allow_package_registries && !wide.allow_package_registries {
        return Some("package registries are not allowed by the wider scope".into());
    }
    None
}

fn egress_key(s: &WorkspaceEgressScope) -> Result<(String, &'static str)> {
    match &s.scope {
        Some(workspace_egress_scope::Scope::Org(o)) if !o.is_empty() => {
            Ok((format!("org:{o}"), "org"))
        }
        Some(workspace_egress_scope::Scope::Repo(r)) => match &r.repo {
            Some(bee_proto::v1::repo_ref::Repo::Id(id)) => Ok((format!("repo:{id}"), "repo")),
            _ => Err(ForgeError::invalid(
                "invalid_scope",
                "resolve the repo to its id first",
            )),
        },
        Some(workspace_egress_scope::Scope::WorkspaceId(w)) if !w.is_empty() => {
            Ok((format!("ws:{w}"), "ws"))
        }
        _ => Err(ForgeError::missing("scope")),
    }
}

fn ws_row(rec: &WorkspaceRec, version: u64) -> Row {
    let w = rec.ws();
    let owner = w.owner.as_ref().map(|o| o.id.clone()).unwrap_or_default();
    Row::new(rec.org_id.clone(), w.id.clone())
        .k1(format!("{owner}/{}", w.name))
        .k2(super::registry::run_repo_ref(&w.repo))
        .n1(w.created_at.as_ref().map(ts_ms).unwrap_or(0) as i64)
        .version(version)
        .msg(rec)
}

/// Cloud workspaces over the `workspaces` surface.
pub struct Workspaces {
    forge: Arc<crate::forge::Forge>,
    changes: Arc<Changes>,
    repo: Arc<dyn AgentRepo>,
    provider: Arc<dyn WorkspaceProvider>,
    config: WorkspaceConfig,
    base_url: String,
    db: Db,
}

impl Workspaces {
    /// Workspaces over `forge`.
    pub fn new(
        forge: Arc<crate::forge::Forge>,
        changes: Arc<Changes>,
        repo: Arc<dyn AgentRepo>,
        provider: Arc<dyn WorkspaceProvider>,
        config: WorkspaceConfig,
        base_url: String,
    ) -> Workspaces {
        Workspaces {
            db: forge.db.clone(),
            forge,
            changes,
            repo,
            provider,
            config,
            base_url,
        }
    }

    /// The deployment's settings.
    pub fn config(&self) -> &WorkspaceConfig {
        &self.config
    }

    fn machine(&self, name: &str) -> Option<MachineType> {
        self.config
            .machine_types
            .iter()
            .find(|m| m.name == name)
            .cloned()
    }

    // ---- org policy ----

    /// The org's workspace policy.
    pub async fn org_policy(&self, cx: &Cx, org_id: &str) -> Result<OrgWorkspacePolicy> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        Ok(tx
            .get_msg::<OrgPolicyRec>(&WS_POLICY, org_id, "policy")
            .await?
            .map(|(p, _)| OrgWorkspacePolicy {
                allowed_machine_types: p.allowed_machine_types,
                allowed_regions: p.allowed_regions,
                allow_public_ports: p.allow_public_ports,
                max_idle_timeout_seconds: p.max_idle_timeout_seconds,
                max_workspaces_per_owner: p.max_workspaces_per_owner,
            })
            .unwrap_or(OrgWorkspacePolicy {
                allow_public_ports: true,
                ..OrgWorkspacePolicy::default()
            }))
    }

    /// Sets the org's workspace policy.
    pub async fn set_org_policy(
        &self,
        cx: &Cx,
        org_id: &str,
        p: OrgWorkspacePolicy,
    ) -> Result<OrgWorkspacePolicy> {
        for m in &p.allowed_machine_types {
            if self.machine(m).is_none() {
                return Err(ForgeError::invalid(
                    "invalid_machine_type",
                    format!("unknown machine type {m}"),
                ));
            }
        }
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        tx.put(
            &WS_POLICY,
            &Row::new(org_id, "policy").msg(&OrgPolicyRec {
                allowed_machine_types: p.allowed_machine_types.clone(),
                allowed_regions: p.allowed_regions.clone(),
                allow_public_ports: p.allow_public_ports,
                max_idle_timeout_seconds: p.max_idle_timeout_seconds,
                max_workspaces_per_owner: p.max_workspaces_per_owner,
            }),
        )
        .await?;
        let json = serde_json::json!({
            "allowed_machine_types": p.allowed_machine_types, "allowed_regions": p.allowed_regions,
            "allow_public_ports": p.allow_public_ports, "max_idle_timeout_seconds": p.max_idle_timeout_seconds,
            "max_workspaces_per_owner": p.max_workspaces_per_owner,
        })
        .to_string();
        tx.emit_event(super::registry::org_event(
            org_id,
            "workspace_policy.updated",
            org_id,
            &json,
        ))
        .await?;
        tx.audit(AuditDraft::new("workspace.org_policy_set", org_id).after(&json))
            .await?;
        tx.commit().await?;
        Ok(p)
    }

    // ---- machine types ----

    /// Machine types for an org (and repo), with prebuild availability for `ref_name`.
    pub async fn machine_types(
        &self,
        cx: &Cx,
        org_id: &str,
        repo: Option<&Repo>,
        ref_name: &str,
        devcontainer_path: &str,
    ) -> Result<Vec<WorkspaceMachineType>> {
        let pol = self.org_policy(cx, org_id).await?;
        let ready = match repo {
            Some(r) => {
                self.ready_prebuilds(cx, r, ref_name, devcontainer_path)
                    .await?
            }
            None => Vec::new(),
        };
        Ok(self
            .config
            .machine_types
            .iter()
            .filter(|m| {
                pol.allowed_machine_types.is_empty() || pol.allowed_machine_types.contains(&m.name)
            })
            .map(|m| WorkspaceMachineType {
                name: m.name.clone(),
                display_name: m.display_name.clone(),
                cpus: m.cpus,
                memory_bytes: m.memory_bytes,
                storage_bytes: m.storage_bytes,
                gpu: m.gpu,
                usd_micros_per_hour: m.usd_micros_per_hour,
                prebuild_available: ready
                    .iter()
                    .any(|p| p.machine_types.is_empty() || p.machine_types.contains(&m.name)),
                regions: m
                    .regions
                    .iter()
                    .filter(|r| pol.allowed_regions.is_empty() || pol.allowed_regions.contains(r))
                    .cloned()
                    .collect(),
            })
            .collect())
    }

    async fn ready_prebuilds(
        &self,
        cx: &Cx,
        repo: &Repo,
        ref_name: &str,
        path: &str,
    ) -> Result<Vec<PrebuildRec>> {
        let r = if ref_name.is_empty() {
            repo.default_ref.as_str()
        } else {
            ref_name
        };
        let p = if path.is_empty() { DEFAULT_PATH } else { path };
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let mut out = Vec::new();
        for (c, _) in tx
            .scan_msgs::<PrebuildConfigRec>(&PREBUILD_CONFIGS, &Scan::scope(repo.id.clone(), 1000))
            .await?
        {
            let cfg = c.config.clone().unwrap_or_default();
            if cfg.r#ref != r || cfg.devcontainer_path != p || !cfg.enabled {
                continue;
            }
            for (pb, _) in tx
                .scan_msgs::<PrebuildRec>(
                    &PREBUILDS,
                    &Scan::scope(cfg.id.clone(), 1000).by_n1().desc(),
                )
                .await?
            {
                if pb
                    .prebuild
                    .as_ref()
                    .is_some_and(|x| x.state == Ps::Ready as i32)
                {
                    out.push(pb);
                }
            }
        }
        Ok(out)
    }

    // ---- workspaces ----

    async fn load(tx: &mut Tx, id: &str) -> Result<(WorkspaceRec, Row)> {
        let org = tx
            .get(&WS_LOCATOR, "ws", id)
            .await?
            .and_then(|r| r.k1)
            .ok_or_else(|| ForgeError::not_found(format!("workspace {id}")))?;
        tx.get_msg::<WorkspaceRec>(&WORKSPACES, &org, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("workspace {id}")))
    }

    /// A workspace with its org id.
    pub async fn get(&self, cx: &Cx, id: &str) -> Result<(Workspace, String)> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, id).await?;
        Ok((rec.ws(), rec.org_id))
    }

    async fn spec_at(
        &self,
        repo: &Repo,
        at: &ObjectId,
        path: &str,
    ) -> Result<(DevcontainerSpec, Option<ObjectId>)> {
        let explicit = !path.is_empty() && path != DEFAULT_PATH;
        let p = if path.is_empty() { DEFAULT_PATH } else { path };
        match self.repo.read_file(repo, at, p).await? {
            Some((blob, bytes)) => Ok((
                DevcontainerSpec::parse(&String::from_utf8_lossy(&bytes))?,
                Some(blob),
            )),
            None if explicit => Err(ForgeError::invalid(
                "invalid_devcontainer",
                format!("{p} does not exist at this ref"),
            )),
            None => Ok((DevcontainerSpec::default_spec(), None)),
        }
    }

    fn pick_machine(&self, spec: &DevcontainerSpec, asked: &str) -> Result<MachineType> {
        let h = spec.host_requirements;
        let fits = |m: &MachineType| {
            m.cpus >= h.cpus
                && m.memory_bytes >= h.memory_bytes
                && m.storage_bytes >= h.storage_bytes
                && (!h.gpu || m.gpu)
        };
        if !asked.is_empty() {
            let m = self.machine(asked).ok_or_else(|| {
                ForgeError::invalid(
                    "invalid_machine_type",
                    format!("unknown machine type {asked}"),
                )
            })?;
            if !fits(&m) {
                return Err(ForgeError::invalid(
                    "machine_too_small",
                    format!("{asked} does not meet the devcontainer's hostRequirements"),
                ));
            }
            return Ok(m);
        }
        self.config
            .machine_types
            .iter()
            .filter(|m| fits(m))
            .min_by_key(|m| (m.usd_micros_per_hour, m.cpus))
            .cloned()
            .ok_or_else(|| {
                ForgeError::invalid(
                    "machine_too_small",
                    "no machine type meets the hostRequirements",
                )
            })
    }

    async fn live_count(&self, tx: &mut Tx, owner: &str) -> Result<u64> {
        tx.count(&WS_LOCATOR, &format!("owner:{owner}"), None).await
    }

    /// Effective egress policy for a workspace in `repo`.
    pub async fn effective_egress(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        ws_id: &str,
    ) -> Result<WorkspaceEgressPolicy> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let mut levels = Vec::new();
        for k in [
            format!("org:{org_id}"),
            format!("repo:{repo_id}"),
            format!("ws:{ws_id}"),
        ] {
            if let Some((p, _)) = tx
                .get_msg::<WorkspaceEgressPolicy>(&EGRESS, &k, "policy")
                .await?
            {
                levels.push(p);
            }
        }
        Ok(merge_egress(&levels))
    }

    async fn build_request(
        &self,
        cx: &Cx,
        rec: &WorkspaceRec,
        repo: &Repo,
    ) -> Result<BuildRequest> {
        let w = rec.ws();
        let owner = w.owner.as_ref().map(|o| o.id.clone()).unwrap_or_default();
        let dot = self.dotfiles(cx, &owner).await?;
        let dotfiles = if dot.enabled {
            dot.repo.and_then(|r| r.repo).map(|r| {
                let name = match r {
                    bee_proto::v1::repo_ref::Repo::Id(i)
                    | bee_proto::v1::repo_ref::Repo::FullName(i) => i,
                };
                (name, dot.install_command.clone())
            })
        } else {
            None
        };
        let prebuild_handle = if w.prebuild_id.is_empty() {
            String::new()
        } else {
            self.prebuild_handle(cx, &w.prebuild_id)
                .await
                .unwrap_or_default()
        };
        Ok(BuildRequest {
            machine: self.machine(&w.machine_type).unwrap_or_default(),
            spec: DevcontainerSpec::from_json(&rec.spec_json),
            egress: self
                .effective_egress(cx, &rec.org_id, &repo.id, &w.id)
                .await?,
            workspace: w,
            repo: repo.clone(),
            prebuild_handle,
            dotfiles,
        })
    }

    async fn prebuild_handle(&self, cx: &Cx, id: &str) -> Result<String> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let cfg = tx
            .get(&WS_LOCATOR, "prebuild", id)
            .await?
            .and_then(|r| r.k1)
            .ok_or_else(|| ForgeError::not_found(format!("prebuild {id}")))?;
        let (p, _) = tx
            .get_msg::<PrebuildRec>(&PREBUILDS, &cfg, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("prebuild {id}")))?;
        Ok(p.prebuild.map(|p| p.log_url).unwrap_or_default())
    }

    /// Creates and provisions a workspace owned by `owner`.
    pub async fn create(
        &self,
        cx: &Cx,
        repo: &Repo,
        owner: &Principal,
        n: NewWorkspace,
    ) -> Result<Workspace> {
        if !n.idempotency_key.is_empty() {
            check_len("idempotency_key", &n.idempotency_key, 256)?;
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            if let Some(id) = tx
                .get(
                    &WS_LOCATOR,
                    &format!("idem:{}", cx.actor_id),
                    &n.idempotency_key,
                )
                .await?
                .and_then(|r| r.k1)
            {
                return Ok(Self::load(&mut tx, &id).await?.0.ws());
            }
        }
        let ref_name = if n.ref_name.trim().is_empty() {
            repo.default_ref.clone()
        } else {
            n.ref_name.trim().to_string()
        };
        let base = self.repo.resolve(repo, &ref_name).await?;
        let path = if n.devcontainer_path.is_empty() {
            DEFAULT_PATH.to_string()
        } else {
            n.devcontainer_path.clone()
        };
        let (spec, _) = self.spec_at(repo, &base, &path).await?;
        let machine = self.pick_machine(&spec, &n.machine_type)?;
        let pol = self.org_policy(cx, &repo.org_id).await?;
        if !pol.allowed_machine_types.is_empty()
            && !pol.allowed_machine_types.contains(&machine.name)
        {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                format!("org policy does not allow machine type {}", machine.name),
            )
            .with_subcode("machine_type_not_allowed"));
        }
        let region = if n.region.is_empty() {
            machine.regions.first().cloned().unwrap_or_default()
        } else {
            n.region.clone()
        };
        if !machine.regions.contains(&region) {
            return Err(ForgeError::invalid(
                "invalid_region",
                format!("{} is not offered in {region}", machine.name),
            ));
        }
        if !pol.allowed_regions.is_empty() && !pol.allowed_regions.contains(&region) {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                format!("org policy does not allow region {region}"),
            )
            .with_subcode("region_not_allowed"));
        }
        let idle = if n.idle_timeout_seconds == 0 {
            self.config.default_idle_timeout_seconds
        } else {
            n.idle_timeout_seconds
        };
        self.check_idle(&pol, idle)?;
        let retention = if n.retention_seconds == 0 {
            self.config.default_retention_seconds
        } else {
            n.retention_seconds
        };
        let sparse: Vec<Vec<u8>> = n
            .sparse_paths
            .iter()
            .map(|p| crate::leases::normalize_path(p).map(String::into_bytes))
            .collect::<Result<_>>()?;
        let prebuild = if n.use_prebuild {
            self.ready_prebuilds(cx, repo, &ref_name, &path)
                .await?
                .into_iter()
                .filter(|p| p.machine_types.is_empty() || p.machine_types.contains(&machine.name))
                .max_by_key(|p| {
                    p.prebuild
                        .as_ref()
                        .and_then(|x| x.finished_at.as_ref().map(ts_ms))
                        .unwrap_or(0)
                })
                .and_then(|p| p.prebuild)
        } else {
            None
        };
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let max = if pol.max_workspaces_per_owner > 0 {
            pol.max_workspaces_per_owner
        } else {
            self.config.max_workspaces_per_owner
        };
        tx.add_n1(
            &WS_LOCATOR,
            &format!("quota_guard:{}", owner.id),
            "workspace_create",
            0,
        )
        .await?;
        let live = self.live_count(&mut tx, &owner.id).await?;
        if live >= max {
            return Err(ForgeError::new(
                ErrorReason::QuotaExceeded,
                format!("{} already has {live} workspaces (limit {max})", owner.id),
            )
            .with_subcode("workspace_quota"));
        }
        let id = tx.new_id()?;
        let name = if n.name.trim().is_empty() {
            format!(
                "{}-{}",
                lc(&repo.name).replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
                id[id.len() - 6..].to_lowercase()
            )
        } else {
            check_ws_name(&n.name)?
        };
        let name = check_ws_name(&name)?;
        if !tx
            .scan(
                &WORKSPACES,
                &Scan::scope(repo.org_id.clone(), 1)
                    .key(crate::store::Key::K1(format!("{}/{name}", owner.id))),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("workspace {name}")));
        }
        let now = tx.now_ms();
        let w = Workspace {
            id: id.clone(),
            name: name.clone(),
            display_name: name.clone(),
            repo: repo_ref_id(&repo.id),
            r#ref: ref_name,
            base_changeset: Some(base),
            workspace_ref: format!("ws/{}/{name}", owner.id),
            devcontainer_path: path,
            machine_type: machine.name.clone(),
            region,
            state: Ws::Provisioning as i32,
            owner: Some(owner.clone()),
            created_by: Some(cx.principal()),
            sparse_paths: sparse,
            idle_timeout_seconds: idle,
            retention_seconds: retention,
            prebuild_id: prebuild.as_ref().map(|p| p.id.clone()).unwrap_or_default(),
            agent_run_id: n.agent_run_id.clone(),
            created_at: Some(ts(now)),
            last_used_at: Some(ts(now)),
            ..Default::default()
        };
        let rec = WorkspaceRec {
            workspace: Some(w.clone()),
            org_id: repo.org_id.clone(),
            spec_json: spec.to_json(),
            ..Default::default()
        };
        tx.insert(&WORKSPACES, &ws_row(&rec, 0)).await?;
        tx.put(
            &WS_LOCATOR,
            &Row::new("ws", id.clone()).k1(repo.org_id.clone()),
        )
        .await?;
        tx.put(
            &WS_LOCATOR,
            &Row::new(format!("owner:{}", owner.id), id.clone()).k1(repo.org_id.clone()),
        )
        .await?;
        if !n.idempotency_key.is_empty() {
            tx.put(
                &WS_LOCATOR,
                &Row::new(format!("idem:{}", cx.actor_id), n.idempotency_key.clone())
                    .k1(id.clone()),
            )
            .await?;
        }
        self.record(&mut tx, &rec, "workspace.created", "workspace.create")
            .await?;
        tx.commit().await?;
        // Provision outside the transaction.
        let req = self.build_request(cx, &rec, repo).await?;
        let res = self.provider.provision(&req).await;
        self.settle(cx, &id, res, Ws::Available, "workspace.provisioned")
            .await
    }

    fn check_idle(&self, pol: &OrgWorkspacePolicy, idle: u64) -> Result<()> {
        if idle > self.config.max_idle_timeout_seconds {
            return Err(ForgeError::invalid(
                "invalid_setting",
                format!(
                    "idle_timeout_seconds is at most {}",
                    self.config.max_idle_timeout_seconds
                ),
            ));
        }
        if pol.max_idle_timeout_seconds > 0 && idle > pol.max_idle_timeout_seconds {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                format!(
                    "org policy limits idle timeouts to {} seconds",
                    pol.max_idle_timeout_seconds
                ),
            )
            .with_subcode("idle_timeout_not_allowed"));
        }
        Ok(())
    }

    /// Applies a provider result: `ok` state with ports and handle, or `FAILED`.
    async fn settle(
        &self,
        cx: &Cx,
        id: &str,
        res: Result<super::ports::Provisioned>,
        ok: Ws,
        action: &str,
    ) -> Result<Workspace> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, id).await?;
        let mut w = rec.ws();
        let now = tx.now_ms();
        match res {
            Ok(p) => {
                w.state = ok as i32;
                w.failure_reason.clear();
                w.last_used_at = Some(ts(now));
                w.stopped_at = None;
                w.retention_expires_at = None;
                if !p.handle.is_empty() {
                    rec.handle = p.handle;
                }
                let old = rec.ports.clone();
                rec.ports = p
                    .ports
                    .iter()
                    .map(|(port, label, protocol)| WorkspacePort {
                        port: *port,
                        label: label.clone(),
                        protocol: if protocol.is_empty() {
                            "http".into()
                        } else {
                            protocol.clone()
                        },
                        visibility: old
                            .iter()
                            .find(|o| o.port == *port)
                            .map(|o| o.visibility)
                            .unwrap_or(WorkspacePortVisibility::Private as i32),
                        forward_url: format!("{}/workspaces/{}/ports/{port}", self.base_url, w.id),
                    })
                    .collect();
                rec.started_ms = now;
            }
            Err(e) => {
                w.state = Ws::Failed as i32;
                w.failure_reason = e.reason_name().to_string();
            }
        }
        rec.workspace = Some(w.clone());
        tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
        self.record(&mut tx, &rec, "workspace.state_changed", action)
            .await?;
        tx.commit().await?;
        Ok(w)
    }

    async fn record(
        &self,
        tx: &mut Tx,
        rec: &WorkspaceRec,
        kind: &str,
        action: &str,
    ) -> Result<()> {
        let w = rec.ws();
        let repo_id = super::registry::run_repo_ref(&w.repo);
        let json = pjson(&w);
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: repo_id.clone(),
            org_id: rec.org_id.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::Repo as i32,
                repo: repo_ref_id(&repo_id),
                id: w.id.clone(),
            }),
            payload_json: json.clone(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new(action, &rec.org_id)
                .repo_id(&repo_id)
                .target_id(&w.id)
                .after(&json),
        )
        .await
    }

    /// Workspaces of a repo, an org or an owner, filtered by state, newest first.
    #[allow(clippy::too_many_arguments)]
    pub async fn list(
        &self,
        cx: &Cx,
        repo_id: &str,
        org_id: &str,
        owner_id: &str,
        states: &[i32],
        after: &str,
        limit: usize,
    ) -> Result<(Vec<(Workspace, String)>, String)> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let mut recs: Vec<WorkspaceRec> = Vec::new();
        if !org_id.is_empty() {
            let scan = if repo_id.is_empty() {
                Scan::scope(org_id, crate::store::MAX_SCAN)
            } else {
                Scan::scope(org_id, crate::store::MAX_SCAN)
                    .key(crate::store::Key::K2(repo_id.to_string()))
            };
            recs = tx
                .scan_msgs::<WorkspaceRec>(&WORKSPACES, &scan)
                .await?
                .into_iter()
                .map(|(r, _)| r)
                .collect();
        } else if !owner_id.is_empty() {
            for r in tx
                .scan(
                    &WS_LOCATOR,
                    &Scan::scope(format!("owner:{owner_id}"), crate::store::MAX_SCAN),
                )
                .await?
            {
                if let Ok((w, _)) = Self::load(&mut tx, &r.id).await {
                    recs.push(w);
                }
            }
        }
        let mut v: Vec<(Workspace, String)> = recs
            .into_iter()
            .map(|r| (r.ws(), r.org_id))
            .filter(|(w, _)| {
                owner_id.is_empty() || w.owner.as_ref().is_some_and(|o| o.id == owner_id)
            })
            .filter(|(w, _)| {
                repo_id.is_empty() || super::registry::run_repo_ref(&w.repo) == repo_id
            })
            .filter(|(w, _)| states.is_empty() || states.contains(&w.state))
            .filter(|(w, _)| after.is_empty() || w.id.as_str() < after)
            .collect();
        v.sort_by(|a, b| b.0.id.cmp(&a.0.id));
        let more = v.len() > limit;
        v.truncate(limit);
        let next = if more {
            v.last().map(|w| w.0.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((v, next))
    }

    /// Updates settings; a machine type change applies at the next start.
    pub async fn update(
        &self,
        cx: &Cx,
        id: &str,
        display_name: Option<String>,
        machine_type: Option<String>,
        idle_timeout_seconds: Option<u64>,
        retention_seconds: Option<u64>,
    ) -> Result<Workspace> {
        let (cur, org) = self.get(cx, id).await?;
        let pol = self.org_policy(cx, &org).await?;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, id).await?;
        let mut w = rec.ws();
        if let Some(d) = display_name {
            w.display_name = check_name("display_name", &d)?;
        }
        if let Some(m) = machine_type {
            let spec = DevcontainerSpec::from_json(&rec.spec_json);
            let machine = self.pick_machine(&spec, &m)?;
            if !pol.allowed_machine_types.is_empty()
                && !pol.allowed_machine_types.contains(&machine.name)
            {
                return Err(ForgeError::new(
                    ErrorReason::PolicyDenied,
                    format!("org policy does not allow machine type {m}"),
                ));
            }
            if w.state == Ws::Stopped as i32 {
                w.machine_type = machine.name;
                rec.next_machine_type.clear();
            } else {
                rec.next_machine_type = machine.name;
            }
        }
        if let Some(i) = idle_timeout_seconds {
            let i = if i == 0 {
                self.config.default_idle_timeout_seconds
            } else {
                i
            };
            self.check_idle(&pol, i)?;
            w.idle_timeout_seconds = i;
        }
        if let Some(r) = retention_seconds {
            w.retention_seconds = if r == 0 {
                self.config.default_retention_seconds
            } else {
                r
            };
            if let Some(s) = &w.stopped_at {
                w.retention_expires_at = Some(ts(ts_ms(s) + w.retention_seconds * 1000));
            }
        }
        let _ = cur;
        rec.workspace = Some(w.clone());
        tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
        self.record(&mut tx, &rec, "workspace.updated", "workspace.update")
            .await?;
        tx.commit().await?;
        Ok(w)
    }

    /// Starts a stopped workspace.
    pub async fn start(&self, cx: &Cx, id: &str) -> Result<Workspace> {
        let (rec, repo) = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            let (mut rec, row) = Self::load(&mut tx, id).await?;
            let mut w = rec.ws();
            if w.state != Ws::Stopped as i32 {
                return Err(ForgeError::invalid_state(format!(
                    "workspace {} is not stopped",
                    w.name
                )));
            }
            if !rec.next_machine_type.is_empty() {
                w.machine_type = std::mem::take(&mut rec.next_machine_type);
            }
            w.state = Ws::Starting as i32;
            rec.workspace = Some(w.clone());
            tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
            self.record(&mut tx, &rec, "workspace.state_changed", "workspace.start")
                .await?;
            tx.commit().await?;
            let repo = self
                .forge
                .orgs
                .repo_any(cx, &super::registry::run_repo_ref(&w.repo))
                .await?;
            (rec, repo)
        };
        let req = self.build_request(cx, &rec, &repo).await?;
        let res = self.provider.start(&rec.handle, &req).await;
        self.settle(cx, id, res, Ws::Available, "workspace.started")
            .await
    }

    /// Stops a running workspace (disk kept; usage recorded).
    pub async fn stop(&self, cx: &Cx, id: &str) -> Result<Workspace> {
        let handle = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            let (mut rec, row) = Self::load(&mut tx, id).await?;
            let mut w = rec.ws();
            if w.state != Ws::Available as i32 {
                return Err(ForgeError::invalid_state(format!(
                    "workspace {} is not running",
                    w.name
                )));
            }
            w.state = Ws::Stopping as i32;
            rec.workspace = Some(w);
            tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
            tx.commit().await?;
            rec.handle
        };
        let dirty = !self
            .provider
            .edits(&handle)
            .await
            .map(|e| e.edits)
            .unwrap_or_default()
            .is_empty();
        self.provider.stop(&handle).await?;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, id).await?;
        let mut w = rec.ws();
        let now = tx.now_ms();
        self.add_usage(&mut tx, &rec, now).await?;
        w.state = Ws::Stopped as i32;
        w.stopped_at = Some(ts(now));
        w.retention_expires_at = Some(ts(now + w.retention_seconds * 1000));
        w.has_unexported_changes = dirty;
        rec.connection_token_hash.clear();
        rec.workspace = Some(w.clone());
        tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
        self.record(&mut tx, &rec, "workspace.state_changed", "workspace.stop")
            .await?;
        tx.commit().await?;
        Ok(w)
    }

    async fn add_usage(&self, tx: &mut Tx, rec: &WorkspaceRec, now: u64) -> Result<()> {
        if rec.started_ms == 0 || now <= rec.started_ms {
            return Ok(());
        }
        let w = rec.ws();
        let m = self.machine(&w.machine_type).unwrap_or_default();
        let secs = (now - rec.started_ms) / 1000;
        let u = UsageRec {
            principal_id: w.owner.as_ref().map(|o| o.id.clone()).unwrap_or_default(),
            machine_type: m.name.clone(),
            core_seconds: secs * u64::from(m.cpus),
            storage_byte_hours: m.storage_bytes / 3600 * secs,
            usd_micros: m.usd_micros_per_hour * secs / 3600,
            prebuild: false,
            subject_id: w.id.clone(),
        };
        self.insert_usage(
            tx,
            &rec.org_id,
            &super::registry::run_repo_ref(&w.repo),
            u,
            now,
        )
        .await
    }

    async fn insert_usage(
        &self,
        tx: &mut Tx,
        org_id: &str,
        repo_id: &str,
        u: UsageRec,
        now: u64,
    ) -> Result<()> {
        let id = tx.new_id()?;
        tx.insert(
            &WS_USAGE,
            &Row::new(org_id, id.clone())
                .k1(u.principal_id.clone())
                .n1(now as i64)
                .msg(&u),
        )
        .await?;
        let hours = u.core_seconds as f64 / 3600.0;
        tx.emit_event(Event {
            kind: "usage.recorded".into(),
            repo_id: repo_id.to_string(),
            org_id: org_id.to_string(),
            principal: u.principal_id.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::Repo as i32,
                repo: repo_ref_id(repo_id),
                id: u.subject_id.clone(),
            }),
            payload_json: serde_json::json!({
                "product": "workspaces_hours",
                "quantity": format!("{hours:.6}"),
                "unit": "core_hours",
                "sku": u.machine_type,
                "principal_id": u.principal_id,
                "repo_id": repo_id,
                "key": format!("workspace_usage:{id}"),
            })
            .to_string(),
            ..Default::default()
        })
        .await
    }

    /// Deletes a workspace; with unexported changes only when `force`.
    pub async fn delete(&self, cx: &Cx, id: &str, force: bool) -> Result<()> {
        let (rec, _) = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            Self::load(&mut tx, id).await?
        };
        let w = rec.ws();
        let dirty = w.has_unexported_changes
            || !self
                .provider
                .edits(&rec.handle)
                .await
                .map(|e| e.edits)
                .unwrap_or_default()
                .is_empty();
        if dirty && !force {
            return Err(ForgeError::invalid_state(format!(
                "workspace {} has unexported changes; export them or delete with force",
                w.name
            ))
            .with_subcode("unexported_changes"));
        }
        self.provider.destroy(&rec.handle).await?;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, _) = Self::load(&mut tx, id).await?;
        let now = tx.now_ms();
        if rec.ws().state == Ws::Available as i32 {
            self.add_usage(&mut tx, &rec, now).await?;
        }
        let mut w = rec.ws();
        w.state = Ws::Deleted as i32;
        rec.workspace = Some(w.clone());
        let owner = w.owner.as_ref().map(|o| o.id.clone()).unwrap_or_default();
        tx.delete(&WORKSPACES, &rec.org_id, id).await?;
        tx.delete(&WS_LOCATOR, "ws", id).await?;
        tx.delete(&WS_LOCATOR, &format!("owner:{owner}"), id)
            .await?;
        tx.delete(&EGRESS, &format!("ws:{id}"), "policy").await?;
        self.record(
            &mut tx,
            &rec,
            "workspace.deleted",
            if force && dirty {
                "workspace.delete_forced"
            } else {
                "workspace.delete"
            },
        )
        .await?;
        tx.commit().await
    }

    /// Rebuilds the container from the devcontainer config, keeping the working copy.
    pub async fn rebuild(&self, cx: &Cx, id: &str, full: bool) -> Result<Workspace> {
        let (rec, repo) = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            let (mut rec, row) = Self::load(&mut tx, id).await?;
            let mut w = rec.ws();
            if w.state != Ws::Available as i32
                && w.state != Ws::Stopped as i32
                && w.state != Ws::Failed as i32
            {
                return Err(ForgeError::invalid_state(format!(
                    "workspace {} cannot be rebuilt now",
                    w.name
                )));
            }
            let repo = self
                .forge
                .orgs
                .repo_any(cx, &super::registry::run_repo_ref(&w.repo))
                .await?;
            let base = w.base_changeset.clone().unwrap_or_default();
            let (spec, _) = self.spec_at(&repo, &base, &w.devcontainer_path).await?;
            rec.spec_json = spec.to_json();
            w.state = Ws::Rebuilding as i32;
            rec.workspace = Some(w);
            tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
            self.record(
                &mut tx,
                &rec,
                "workspace.state_changed",
                if full {
                    "workspace.rebuild_full"
                } else {
                    "workspace.rebuild"
                },
            )
            .await?;
            tx.commit().await?;
            (rec, repo)
        };
        let req = self.build_request(cx, &rec, &repo).await?;
        let res = self.provider.rebuild(&rec.handle, &req, full).await;
        self.settle(cx, id, res, Ws::Available, "workspace.rebuilt")
            .await
    }

    /// Snapshots the working copy onto the workspace ref; opens a CR when asked.
    pub async fn export(
        &self,
        cx: &Cx,
        id: &str,
        message: &str,
        provenance: Option<Provenance>,
        open_change_request: bool,
        target_ref: &str,
    ) -> Result<(ObjectId, Ref, String)> {
        let (rec, _) = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            Self::load(&mut tx, id).await?
        };
        let w = rec.ws();
        if w.state != Ws::Available as i32 && w.state != Ws::Stopped as i32 {
            return Err(ForgeError::invalid_state(format!(
                "workspace {} has no working copy now",
                w.name
            )));
        }
        let edits = self.provider.edits(&rec.handle).await?.edits;
        if edits.is_empty() {
            return Err(ForgeError::new(
                ErrorReason::EmptyChange,
                "nothing changed in the workspace",
            ));
        }
        let repo = self
            .forge
            .orgs
            .repo_any(cx, &super::registry::run_repo_ref(&w.repo))
            .await?;
        let parent = match self.repo.resolve(&repo, &w.workspace_ref).await {
            Ok(t) => t,
            Err(_) => w.base_changeset.clone().unwrap_or_default(),
        };
        let msg = if message.trim().is_empty() {
            format!("Export workspace {}", w.name)
        } else {
            message.to_string()
        };
        let changeset = self
            .repo
            .commit(&repo, &parent, edits.clone(), &msg, &cx.chain)
            .await?;
        let version = self
            .repo
            .set_ref(&repo, &w.workspace_ref, &changeset)
            .await?;
        self.provider.rebase(&rec.handle).await?;
        let mut number = String::new();
        if open_change_request {
            let cr = self
                .changes
                .open(
                    cx,
                    &repo,
                    NewChange {
                        target_ref: if target_ref.is_empty() {
                            w.r#ref.clone()
                        } else {
                            target_ref.to_string()
                        },
                        head: Some(changeset.clone()),
                        title: msg.lines().next().unwrap_or("").to_string(),
                        body: msg
                            .lines()
                            .skip(1)
                            .collect::<Vec<_>>()
                            .join("\n")
                            .trim()
                            .to_string(),
                        ..NewChange::default()
                    },
                )
                .await?;
            number = cr.number.to_string();
        }
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, id).await?;
        let mut w = rec.ws();
        w.has_unexported_changes = false;
        w.last_used_at = Some(ts(tx.now_ms()));
        rec.workspace = Some(w.clone());
        tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
        let json = serde_json::json!({
            "workspace_id": w.id, "changeset": hex(&changeset.raw), "ref": w.workspace_ref,
            "files": edits.len(), "change_request": number,
            "provenance": provenance.as_ref().map(|p| serde_json::from_str::<serde_json::Value>(&pjson(p)).unwrap_or_default()),
        })
        .to_string();
        let repo_id = repo.id.clone();
        tx.emit_event(Event {
            kind: "workspace.exported".into(),
            repo_id: repo_id.clone(),
            org_id: rec.org_id.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::Repo as i32,
                repo: repo_ref_id(&repo_id),
                id: w.id.clone(),
            }),
            payload_json: json.clone(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("workspace.export", &rec.org_id)
                .repo_id(&repo_id)
                .target_id(&w.id)
                .after(&json),
        )
        .await?;
        tx.commit().await?;
        Ok((
            changeset.clone(),
            Ref {
                name: w.workspace_ref.clone(),
                target: Some(changeset),
                version,
            },
            number,
        ))
    }

    /// Connection endpoints and a short-lived token for a running workspace.
    pub async fn connection(&self, cx: &Cx, id: &str) -> Result<WorkspaceConnection> {
        let (rec, _) = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            Self::load(&mut tx, id).await?
        };
        let w = rec.ws();
        if w.state != Ws::Available as i32 {
            return Err(ForgeError::invalid_state(format!(
                "workspace {} is not running",
                w.name
            )));
        }
        let ep = self.provider.endpoints(&rec.handle, &w).await?;
        let token = self.db.ids().random_hex(32)?;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, id).await?;
        let now = tx.now_ms();
        let exp = now + self.config.connection_token_ttl_seconds * 1000;
        rec.connection_token_hash = Sha256::digest(token.as_bytes()).to_vec();
        rec.token_expires_ms = exp;
        let mut w = rec.ws();
        w.last_used_at = Some(ts(now));
        rec.workspace = Some(w.clone());
        tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
        tx.audit(
            AuditDraft::new("workspace.connect", &rec.org_id)
                .repo_id(&super::registry::run_repo_ref(&w.repo))
                .target_id(id),
        )
        .await?;
        tx.commit().await?;
        Ok(WorkspaceConnection {
            workspace_id: id.to_string(),
            web_ide_url: ep.web_ide_url,
            ssh_host: ep.ssh_host,
            ssh_port: ep.ssh_port,
            ssh_user: ep.ssh_user,
            connection_token: token,
            token_expires_at: Some(ts(exp)),
        })
    }

    /// Whether `token` is the workspace's current, unexpired connection token (the server's
    /// connection proxy calls this).
    pub async fn verify_connection_token(&self, cx: &Cx, id: &str, token: &str) -> Result<bool> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, id).await?;
        let h = Sha256::digest(token.as_bytes()).to_vec();
        Ok(!rec.connection_token_hash.is_empty()
            && h.len() == rec.connection_token_hash.len()
            && h.iter()
                .zip(&rec.connection_token_hash)
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
            && tx.now_ms() < rec.token_expires_ms
            && rec.ws().state == Ws::Available as i32)
    }

    /// Forwarded ports.
    pub async fn ports(&self, cx: &Cx, id: &str) -> Result<Vec<WorkspacePort>> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, id).await?;
        Ok(if rec.ws().state == Ws::Available as i32 {
            rec.ports
        } else {
            Vec::new()
        })
    }

    /// Sets a port's visibility.
    pub async fn set_port_visibility(
        &self,
        cx: &Cx,
        id: &str,
        port: u32,
        vis: WorkspacePortVisibility,
    ) -> Result<WorkspacePort> {
        if vis == WorkspacePortVisibility::Unspecified {
            return Err(ForgeError::missing("visibility"));
        }
        let (_, org) = self.get(cx, id).await?;
        let pol = self.org_policy(cx, &org).await?;
        if vis == WorkspacePortVisibility::Public && !pol.allow_public_ports {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                "org policy forbids public ports",
            )
            .with_subcode("public_ports_not_allowed"));
        }
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, id).await?;
        let p = rec
            .ports
            .iter_mut()
            .find(|p| p.port == port)
            .ok_or_else(|| ForgeError::not_found(format!("port {port}")))?;
        p.visibility = vis as i32;
        let out = p.clone();
        tx.update(&WORKSPACES, &ws_row(&rec, row.version)).await?;
        let w = rec.ws();
        let repo_id = super::registry::run_repo_ref(&w.repo);
        let json = pjson(&out);
        tx.emit_event(Event {
            kind: "workspace_port.updated".into(),
            repo_id: repo_id.clone(),
            org_id: rec.org_id.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::Repo as i32,
                repo: repo_ref_id(&repo_id),
                id: id.to_string(),
            }),
            payload_json: json.clone(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("workspace.port_visibility", &rec.org_id)
                .repo_id(&repo_id)
                .target_id(id)
                .after(&json),
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    // ---- agent runs ----

    /// A workspace for an agent run (owned by the agent, named after the run).
    pub async fn create_for_run(
        &self,
        cx: &Cx,
        repo: &Repo,
        agent: &Principal,
        run: &bee_proto::v1::AgentRun,
        machine_type: &str,
    ) -> Result<Workspace> {
        let w = self
            .create(
                cx,
                repo,
                agent,
                NewWorkspace {
                    name: format!("run-{}", run.id.to_lowercase()),
                    ref_name: run.base_ref.clone(),
                    machine_type: machine_type.to_string(),
                    use_prebuild: true,
                    agent_run_id: run.id.clone(),
                    idempotency_key: format!("run:{}", run.id),
                    ..NewWorkspace::default()
                },
            )
            .await?;
        if w.state == Ws::Failed as i32 {
            return Err(ForgeError::unavailable(format!(
                "workspace provisioning failed: {}",
                w.failure_reason
            )));
        }
        Ok(w)
    }

    /// Stops a run's workspace when the run ends.
    pub async fn stop_for_run(&self, cx: &Cx, id: &str) -> Result<()> {
        match self.stop(cx, id).await {
            Ok(_) => Ok(()),
            Err(e) if e.reason == ErrorReason::InvalidState => Ok(()),
            Err(e) => Err(e),
        }
    }

    // ---- prebuilds ----

    /// Creates a prebuild configuration.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_prebuild_config(
        &self,
        cx: &Cx,
        repo: &Repo,
        mut c: WorkspacePrebuildConfig,
    ) -> Result<WorkspacePrebuildConfig> {
        self.check_prebuild_config(&c)?;
        if c.r#ref.is_empty() {
            c.r#ref = repo.default_ref.clone();
        }
        if c.devcontainer_path.is_empty() {
            c.devcontainer_path = DEFAULT_PATH.into();
        }
        if c.retained_versions == 0 {
            c.retained_versions = 2;
        }
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        if tx.count(&PREBUILD_CONFIGS, &repo.id, None).await? >= 50 {
            return Err(ForgeError::too_large("prebuild_configs", 50, 51));
        }
        let now = tx.now_ms();
        c.id = tx.new_id()?;
        c.repo = repo_ref_id(&repo.id);
        c.enabled = true;
        c.created_at = Some(ts(now));
        c.updated_at = Some(ts(now));
        let rec = PrebuildConfigRec {
            config: Some(c.clone()),
            last_scheduled_ms: now,
            last_spec_blob: None,
        };
        tx.insert(
            &PREBUILD_CONFIGS,
            &Row::new(repo.id.clone(), c.id.clone())
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        self.record_prebuild(
            &mut tx,
            repo,
            &c.id,
            &pjson(&c),
            "workspace.prebuild_config_create",
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }

    fn check_prebuild_config(&self, c: &WorkspacePrebuildConfig) -> Result<()> {
        for m in &c.machine_types {
            if self.machine(m).is_none() {
                return Err(ForgeError::invalid(
                    "invalid_machine_type",
                    format!("unknown machine type {m}"),
                ));
            }
        }
        if c.trigger == WorkspacePrebuildTrigger::Scheduled as i32 {
            super::cron::Cron::parse(&c.schedule)?;
        }
        if c.retained_versions > 20 {
            return Err(ForgeError::invalid(
                "invalid_setting",
                "retained_versions is at most 20",
            ));
        }
        Ok(())
    }

    async fn record_prebuild(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        id: &str,
        json: &str,
        action: &str,
    ) -> Result<()> {
        tx.emit_event(Event {
            kind: "workspace_prebuild.updated".into(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::Repo as i32,
                repo: repo_ref_id(&repo.id),
                id: id.to_string(),
            }),
            payload_json: json.to_string(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new(action, &repo.org_id)
                .repo_id(&repo.id)
                .target_id(id)
                .after(json),
        )
        .await
    }

    /// One prebuild configuration.
    pub async fn get_prebuild_config(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<WorkspacePrebuildConfig> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        tx.get_msg::<PrebuildConfigRec>(&PREBUILD_CONFIGS, &repo.id, id)
            .await?
            .and_then(|(c, _)| c.config)
            .ok_or_else(|| ForgeError::not_found(format!("prebuild configuration {id}")))
    }

    /// A repo's prebuild configurations.
    pub async fn list_prebuild_configs(
        &self,
        cx: &Cx,
        repo: &Repo,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<WorkspacePrebuildConfig>, String)> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let mut v: Vec<WorkspacePrebuildConfig> = tx
            .scan_msgs::<PrebuildConfigRec>(&PREBUILD_CONFIGS, &Scan::scope(repo.id.clone(), 1000))
            .await?
            .into_iter()
            .filter_map(|(c, _)| c.config)
            .filter(|c| after.is_empty() || c.id.as_str() > after)
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        let more = v.len() > limit;
        v.truncate(limit);
        let next = if more {
            v.last().map(|c| c.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((v, next))
    }

    /// Updates a prebuild configuration.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_prebuild_config(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        regions: Vec<String>,
        machine_types: Vec<String>,
        trigger: Option<WorkspacePrebuildTrigger>,
        schedule: Option<String>,
        retained_versions: Option<u32>,
        enabled: Option<bool>,
    ) -> Result<WorkspacePrebuildConfig> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, row) = tx
            .get_msg::<PrebuildConfigRec>(&PREBUILD_CONFIGS, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("prebuild configuration {id}")))?;
        let mut c = rec.config.clone().unwrap_or_default();
        if !regions.is_empty() {
            c.regions = regions;
        }
        if !machine_types.is_empty() {
            c.machine_types = machine_types;
        }
        if let Some(t) = trigger {
            c.trigger = t as i32;
        }
        if let Some(s) = schedule {
            c.schedule = s;
        }
        if let Some(r) = retained_versions {
            c.retained_versions = r.max(1);
        }
        if let Some(e) = enabled {
            c.enabled = e;
        }
        self.check_prebuild_config(&c)?;
        c.updated_at = Some(ts(tx.now_ms()));
        rec.config = Some(c.clone());
        tx.update(
            &PREBUILD_CONFIGS,
            &Row::new(repo.id.clone(), id)
                .n1(row.n1.unwrap_or(0))
                .version(row.version)
                .msg(&rec),
        )
        .await?;
        self.record_prebuild(
            &mut tx,
            repo,
            id,
            &pjson(&c),
            "workspace.prebuild_config_update",
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }

    /// Deletes a configuration and its prebuilt images.
    pub async fn delete_prebuild_config(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<()> {
        let builds = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            if tx.get(&PREBUILD_CONFIGS, &repo.id, id).await?.is_none() {
                return Err(ForgeError::not_found(format!(
                    "prebuild configuration {id}"
                )));
            }
            tx.scan_msgs::<PrebuildRec>(&PREBUILDS, &Scan::scope(id, crate::store::MAX_SCAN))
                .await?
        };
        for (p, _) in &builds {
            if let Some(pb) = &p.prebuild
                && !pb.log_url.is_empty()
            {
                let _ = self.provider.delete_prebuild(&pb.log_url).await;
            }
        }
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        for (_, row) in builds {
            tx.delete(&PREBUILDS, id, &row.id).await?;
            tx.delete(&WS_LOCATOR, "prebuild", &row.id).await?;
        }
        tx.delete(&PREBUILD_CONFIGS, &repo.id, id).await?;
        self.record_prebuild(
            &mut tx,
            repo,
            id,
            "{\"deleted\":true}",
            "workspace.prebuild_config_delete",
        )
        .await?;
        tx.commit().await
    }

    /// Queues a prebuild for a configuration and runs it now.
    pub async fn trigger_prebuild(
        &self,
        cx: &Cx,
        repo: &Repo,
        config_id: &str,
    ) -> Result<WorkspacePrebuild> {
        let id = self.queue_prebuild(cx, repo, config_id).await?;
        self.run_prebuild(cx, config_id, &id).await
    }

    async fn queue_prebuild(&self, cx: &Cx, repo: &Repo, config_id: &str) -> Result<String> {
        let cfg = self.get_prebuild_config(cx, repo, config_id).await?;
        let at = self.repo.resolve(repo, &cfg.r#ref).await?;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let now = tx.now_ms();
        let id = tx.new_id()?;
        let p = WorkspacePrebuild {
            id: id.clone(),
            config_id: config_id.to_string(),
            repo: repo_ref_id(&repo.id),
            changeset: Some(at),
            region: cfg
                .regions
                .first()
                .cloned()
                .unwrap_or_else(|| "local".into()),
            state: Ps::Queued as i32,
            created_at: Some(ts(now)),
            ..Default::default()
        };
        let rec = PrebuildRec {
            prebuild: Some(p.clone()),
            spec_blob: None,
            machine_types: cfg.machine_types.clone(),
        };
        tx.insert(
            &PREBUILDS,
            &Row::new(config_id, id.clone())
                .k1(repo.id.clone())
                .k2("queued")
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        tx.put(&WS_LOCATOR, &Row::new("prebuild", id.clone()).k1(config_id))
            .await?;
        self.record_prebuild(&mut tx, repo, &id, &pjson(&p), "workspace.prebuild_queue")
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    async fn run_prebuild(&self, cx: &Cx, config_id: &str, id: &str) -> Result<WorkspacePrebuild> {
        let (rec, repo) = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            let (mut rec, row) = tx
                .get_msg::<PrebuildRec>(&PREBUILDS, config_id, id)
                .await?
                .ok_or_else(|| ForgeError::not_found(format!("prebuild {id}")))?;
            let mut p = rec.prebuild.clone().unwrap_or_default();
            if p.state != Ps::Queued as i32 {
                return Ok(p);
            }
            p.state = Ps::Running as i32;
            rec.prebuild = Some(p.clone());
            tx.update(
                &PREBUILDS,
                &Row::new(config_id, id)
                    .k1(row.k1.clone().unwrap_or_default())
                    .k2("running")
                    .n1(row.n1.unwrap_or(0))
                    .version(row.version)
                    .msg(&rec),
            )
            .await?;
            tx.commit().await?;
            let repo = self
                .forge
                .orgs
                .repo_any(cx, &super::registry::run_repo_ref(&p.repo))
                .await?;
            (rec, repo)
        };
        let p = rec.prebuild.clone().unwrap_or_default();
        let cfg = self.get_prebuild_config(cx, &repo, config_id).await?;
        let started = self.db.now_ms();
        let at = p.changeset.clone().unwrap_or_default();
        let outcome = async {
            let (spec, blob) = self.spec_at(&repo, &at, &cfg.devcontainer_path).await?;
            let machine_name = cfg.machine_types.first().cloned().unwrap_or_default();
            let machine = self.pick_machine(&spec, &machine_name)?;
            let req = BuildRequest {
                workspace: Workspace {
                    id: id.to_string(),
                    repo: repo_ref_id(&repo.id),
                    r#ref: cfg.r#ref.clone(),
                    base_changeset: Some(at.clone()),
                    devcontainer_path: cfg.devcontainer_path.clone(),
                    machine_type: machine.name.clone(),
                    ..Default::default()
                },
                repo: repo.clone(),
                spec,
                machine: machine.clone(),
                egress: self
                    .effective_egress(cx, &repo.org_id, &repo.id, "")
                    .await?,
                ..BuildRequest::default()
            };
            let handle = self.provider.prebuild(&req, &at).await?;
            Ok::<_, ForgeError>((handle, blob, machine))
        }
        .await;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let (mut rec, row) = tx
            .get_msg::<PrebuildRec>(&PREBUILDS, config_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("prebuild {id}")))?;
        let mut p = rec.prebuild.clone().unwrap_or_default();
        let now = tx.now_ms();
        p.finished_at = Some(ts(now));
        p.duration_seconds = (now.saturating_sub(started)) / 1000;
        let mut machine_for_usage = None;
        match outcome {
            Ok((handle, blob, machine)) => {
                p.state = Ps::Ready as i32;
                p.log_url = handle;
                rec.spec_blob = blob.clone();
                machine_for_usage = Some(machine);
                if let Some((mut c, crow)) = tx
                    .get_msg::<PrebuildConfigRec>(&PREBUILD_CONFIGS, &repo.id, config_id)
                    .await?
                {
                    c.last_spec_blob = blob;
                    tx.update(
                        &PREBUILD_CONFIGS,
                        &Row::new(repo.id.clone(), config_id)
                            .n1(crow.n1.unwrap_or(0))
                            .version(crow.version)
                            .msg(&c),
                    )
                    .await?;
                }
            }
            Err(e) => {
                p.state = Ps::Failed as i32;
                p.failure_reason = format!("{}: {}", e.reason_name(), e.message);
            }
        }
        let state_name = if p.state == Ps::Ready as i32 {
            "ready"
        } else {
            "failed"
        };
        rec.prebuild = Some(p.clone());
        tx.update(
            &PREBUILDS,
            &Row::new(config_id, id)
                .k1(repo.id.clone())
                .k2(state_name)
                .n1(row.n1.unwrap_or(0))
                .version(row.version)
                .msg(&rec),
        )
        .await?;
        if let Some(m) = machine_for_usage {
            let secs = p.duration_seconds.max(1);
            self.insert_usage(
                &mut tx,
                &repo.org_id,
                &repo.id,
                UsageRec {
                    principal_id: String::new(),
                    machine_type: m.name.clone(),
                    core_seconds: secs * u64::from(m.cpus),
                    storage_byte_hours: m.storage_bytes / 3600 * secs,
                    usd_micros: m.usd_micros_per_hour * secs / 3600,
                    prebuild: true,
                    subject_id: id.to_string(),
                },
                now,
            )
            .await?;
            // Keep the newest `retained_versions` ready prebuilds.
            let mut ready: Vec<(PrebuildRec, Row)> = tx
                .scan_msgs::<PrebuildRec>(
                    &PREBUILDS,
                    &Scan::scope(config_id, crate::store::MAX_SCAN)
                        .by_n1()
                        .desc(),
                )
                .await?
                .into_iter()
                .filter(|(r, _)| {
                    r.prebuild
                        .as_ref()
                        .is_some_and(|x| x.state == Ps::Ready as i32)
                })
                .collect();
            let keep = cfg.retained_versions.max(1) as usize;
            for (mut old, orow) in ready.drain(..).skip(keep) {
                let mut op = old.prebuild.clone().unwrap_or_default();
                op.state = Ps::Superseded as i32;
                if !op.log_url.is_empty() {
                    let _ = self.provider.delete_prebuild(&op.log_url).await;
                }
                old.prebuild = Some(op);
                tx.update(
                    &PREBUILDS,
                    &Row::new(config_id, orow.id.clone())
                        .k1(repo.id.clone())
                        .k2("superseded")
                        .n1(orow.n1.unwrap_or(0))
                        .version(orow.version)
                        .msg(&old),
                )
                .await?;
            }
        }
        self.record_prebuild(&mut tx, &repo, id, &pjson(&p), "workspace.prebuild_finish")
            .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Prebuilds of a configuration, newest first.
    pub async fn list_prebuilds(
        &self,
        cx: &Cx,
        repo: &Repo,
        config_id: &str,
        state: i32,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<WorkspacePrebuild>, String)> {
        self.get_prebuild_config(cx, repo, config_id).await?;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let mut v: Vec<WorkspacePrebuild> = tx
            .scan_msgs::<PrebuildRec>(&PREBUILDS, &Scan::scope(config_id, crate::store::MAX_SCAN))
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.prebuild)
            .filter(|p| state == 0 || p.state == state)
            .filter(|p| after.is_empty() || p.id.as_str() < after)
            .collect();
        v.sort_by(|a, b| b.id.cmp(&a.id));
        let more = v.len() > limit;
        v.truncate(limit);
        let next = if more {
            v.last().map(|p| p.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((v, next))
    }

    /// A ref advanced: queue prebuilds for configurations that build on every land, or on
    /// lands that change the devcontainer file.
    pub async fn on_ref_advanced(&self, cx: &Cx, repo: &Repo, ref_name: &str) -> Result<usize> {
        let configs = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            tx.scan_msgs::<PrebuildConfigRec>(
                &PREBUILD_CONFIGS,
                &Scan::scope(repo.id.clone(), 1000),
            )
            .await?
        };
        let mut n = 0;
        for (c, _) in configs {
            let cfg = c.config.clone().unwrap_or_default();
            if !cfg.enabled || cfg.r#ref != ref_name {
                continue;
            }
            let fire = match WorkspacePrebuildTrigger::try_from(cfg.trigger)
                .unwrap_or(WorkspacePrebuildTrigger::Unspecified)
            {
                WorkspacePrebuildTrigger::EveryLand => true,
                WorkspacePrebuildTrigger::ConfigChange => {
                    let at = self.repo.resolve(repo, ref_name).await?;
                    let blob = self
                        .repo
                        .read_file(repo, &at, &cfg.devcontainer_path)
                        .await?
                        .map(|(b, _)| b);
                    blob != c.last_spec_blob
                }
                _ => false,
            };
            if fire {
                self.queue_prebuild(cx, repo, &cfg.id).await?;
                n += 1;
            }
        }
        Ok(n)
    }

    // ---- dotfiles, egress, usage ----

    /// A principal's dotfiles setting.
    pub async fn dotfiles(&self, cx: &Cx, principal: &str) -> Result<WorkspaceDotfiles> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        Ok(tx
            .get_msg::<WorkspaceDotfiles>(&DOTFILES, "", principal)
            .await?
            .map(|(d, _)| d)
            .unwrap_or(WorkspaceDotfiles {
                principal_id: principal.to_string(),
                ..Default::default()
            }))
    }

    /// Sets a principal's dotfiles setting (`org_id` for the audit entry).
    pub async fn set_dotfiles(
        &self,
        cx: &Cx,
        org_id: &str,
        mut d: WorkspaceDotfiles,
    ) -> Result<WorkspaceDotfiles> {
        check_len("install_command", &d.install_command, 1024)?;
        if d.enabled && d.repo.as_ref().is_none_or(|r| r.repo.is_none()) {
            return Err(ForgeError::missing("repo"));
        }
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        tx.put(&DOTFILES, &Row::new("", d.principal_id.clone()).msg(&d))
            .await?;
        d.principal_id = d.principal_id.clone();
        let json = pjson(&d);
        if !org_id.is_empty() {
            tx.emit_event(super::registry::org_event(
                org_id,
                "workspace_dotfiles.updated",
                &d.principal_id,
                &json,
            ))
            .await?;
        }
        tx.audit(
            AuditDraft::new("workspace.dotfiles_set", org_id)
                .target_id(&d.principal_id)
                .after(&json),
        )
        .await?;
        tx.commit().await?;
        Ok(d)
    }

    /// The policy set at a scope, with the effective merged policy.
    pub async fn egress_policy(
        &self,
        cx: &Cx,
        scope: &WorkspaceEgressScope,
        org_id: &str,
        repo_id: &str,
    ) -> Result<WorkspaceEgressPolicy> {
        let (key, kind) = egress_key(scope)?;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let mut p = tx
            .get_msg::<WorkspaceEgressPolicy>(&EGRESS, &key, "policy")
            .await?
            .map(|(p, _)| p)
            .unwrap_or_else(|| WorkspaceEgressPolicy {
                scope: Some(scope.clone()),
                ..Default::default()
            });
        drop(tx);
        let ws = if kind == "ws" {
            key.trim_start_matches("ws:").to_string()
        } else {
            String::new()
        };
        let repo = if kind == "org" { "" } else { repo_id };
        let mut eff = self.effective_egress(cx, org_id, repo, &ws).await?;
        eff.scope = Some(scope.clone());
        p.effective = Some(Box::new(eff));
        Ok(p)
    }

    /// Sets the policy at one scope; narrower scopes may only restrict.
    pub async fn set_egress_policy(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        mut p: WorkspaceEgressPolicy,
    ) -> Result<WorkspaceEgressPolicy> {
        let scope = p.scope.clone().unwrap_or_default();
        let (key, kind) = egress_key(&scope)?;
        if p.allowed_domains.len() > 500
            || p.denied_domains.len() > 500
            || p.allowed_cidrs.len() > 500
        {
            return Err(ForgeError::too_large("egress_rules", 500, 501));
        }
        for d in p.allowed_domains.iter().chain(p.denied_domains.iter()) {
            let t = d.trim_start_matches("*.");
            if t.is_empty() || t.contains('*') || t.contains('/') || t.contains(' ') {
                return Err(ForgeError::invalid(
                    "invalid_domain",
                    format!("bad domain {d:?}"),
                ));
            }
        }
        for c in &p.allowed_cidrs {
            let ok = c.split_once('/').is_some_and(|(ip, bits)| {
                ip.parse::<std::net::IpAddr>().is_ok() && bits.parse::<u8>().is_ok_and(|b| b <= 128)
            });
            if !ok {
                return Err(ForgeError::invalid(
                    "invalid_cidr",
                    format!("bad CIDR {c:?}"),
                ));
            }
        }
        if p.default_action == WorkspaceEgressDefault::Unspecified as i32 {
            p.default_action = WorkspaceEgressDefault::Allow as i32;
        }
        let wide = match kind {
            "org" => None,
            "repo" => Some(self.effective_egress(cx, org_id, "", "").await?),
            _ => Some(self.effective_egress(cx, org_id, repo_id, "").await?),
        };
        if let Some(w) = wide
            && let Some(why) = widens(&w, &p)
        {
            return Err(ForgeError::new(
                ErrorReason::PolicyDenied,
                format!("a narrower scope can only restrict: {why}"),
            )
            .with_subcode("egress_wider_than_scope"));
        }
        p.effective = None;
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        p.updated_at = Some(ts(tx.now_ms()));
        tx.put(&EGRESS, &Row::new(key.clone(), "policy").msg(&p))
            .await?;
        let json = pjson(&p);
        tx.emit_event(Event {
            kind: "workspace_policy.updated".into(),
            repo_id: if kind == "repo" {
                repo_id.to_string()
            } else {
                String::new()
            },
            org_id: org_id.to_string(),
            subject: Some(SubjectRef {
                kind: SubjectKind::Org as i32,
                repo: None,
                id: key.clone(),
            }),
            payload_json: json.clone(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("workspace.egress_set", org_id)
                .repo_id(if kind == "repo" { repo_id } else { "" })
                .target_id(&key)
                .after(&json),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Usage of an org (or one principal) over `[start, end)`.
    pub async fn usage(
        &self,
        cx: &Cx,
        org_id: &str,
        principal: &str,
        start_ms: u64,
        end_ms: u64,
    ) -> Result<WorkspaceUsage> {
        let mut tx = self.db.begin(WS_SURFACE, cx).await?;
        let mut scan = Scan::scope(org_id, crate::store::MAX_SCAN)
            .by_n1()
            .n1_min(start_ms as i64);
        if end_ms < u64::MAX {
            scan = scan.n1_max(end_ms as i64 - 1);
        }
        let mut lines: Vec<WorkspaceUsageLine> = Vec::new();
        let (mut total, mut pre) = (0u64, 0u64);
        for (u, _) in tx.scan_msgs::<UsageRec>(&WS_USAGE, &scan).await? {
            if !principal.is_empty() && u.principal_id != principal {
                continue;
            }
            total += u.usd_micros;
            if u.prebuild {
                pre += u.usd_micros;
            }
            match lines.iter_mut().find(|l| l.machine_type == u.machine_type) {
                Some(l) => {
                    l.core_seconds += u.core_seconds;
                    l.storage_byte_hours += u.storage_byte_hours;
                    l.usd_micros += u.usd_micros;
                }
                None => lines.push(WorkspaceUsageLine {
                    machine_type: u.machine_type.clone(),
                    core_seconds: u.core_seconds,
                    storage_byte_hours: u.storage_byte_hours,
                    usd_micros: u.usd_micros,
                }),
            }
        }
        lines.sort_by(|a, b| a.machine_type.cmp(&b.machine_type));
        Ok(WorkspaceUsage {
            org: org_id.to_string(),
            principal_id: principal.to_string(),
            window_start: (start_ms > 0).then(|| ts(start_ms)),
            window_end: (end_ms < u64::MAX).then(|| ts(end_ms)),
            lines,
            total_usd_micros: total,
            prebuild_usd_micros: pre,
        })
    }

    // ---- maintenance ----

    /// Stops idle workspaces, deletes expired stopped ones, runs queued and scheduled prebuilds.
    pub async fn maintain(&self, cx: &Cx) -> Result<()> {
        let now = self.db.now_ms();
        let all = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            tx.scan_msgs::<WorkspaceRec>(
                &WORKSPACES,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        };
        for (rec, _) in all {
            let w = rec.ws();
            let last = w.last_used_at.as_ref().map(ts_ms).unwrap_or(0);
            if w.state == Ws::Available as i32
                && w.idle_timeout_seconds > 0
                && last + w.idle_timeout_seconds * 1000 <= now
            {
                let _ = self.stop(cx, &w.id).await;
            } else if w.state == Ws::Stopped as i32
                && w.retention_expires_at
                    .as_ref()
                    .is_some_and(|t| ts_ms(t) <= now)
            {
                let _ = self.delete(cx, &w.id, true).await;
            }
        }
        // Scheduled prebuilds.
        let configs = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            tx.scan_msgs::<PrebuildConfigRec>(
                &PREBUILD_CONFIGS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        };
        for (c, row) in configs {
            let cfg = c.config.clone().unwrap_or_default();
            if !cfg.enabled || cfg.trigger != WorkspacePrebuildTrigger::Scheduled as i32 {
                continue;
            }
            let Ok(cron) = super::cron::Cron::parse(&cfg.schedule) else {
                continue;
            };
            if cron.next_between(c.last_scheduled_ms, now).is_none() {
                continue;
            }
            {
                let mut tx = self.db.begin(WS_SURFACE, cx).await?;
                let mut c2 = c.clone();
                c2.last_scheduled_ms = now;
                if tx
                    .update(
                        &PREBUILD_CONFIGS,
                        &Row::new(row.scope.clone(), row.id.clone())
                            .n1(row.n1.unwrap_or(0))
                            .version(row.version)
                            .msg(&c2),
                    )
                    .await
                    .is_err()
                {
                    continue;
                }
                tx.commit().await?;
            }
            if let Ok(repo) = self.forge.orgs.repo_any(cx, &row.scope).await {
                let _ = self.queue_prebuild(cx, &repo, &cfg.id).await;
            }
        }
        // Queued prebuilds.
        let queued = {
            let mut tx = self.db.begin(WS_SURFACE, cx).await?;
            tx.scan(
                &PREBUILDS,
                &Scan {
                    key: Some(crate::store::Key::K2("queued".into())),
                    limit: 100,
                    ..Scan::default()
                },
            )
            .await?
        };
        for r in queued {
            let _ = self.run_prebuild(cx, &r.scope, &r.id).await;
        }
        Ok(())
    }
}
