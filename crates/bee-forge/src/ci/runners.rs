//! Runners, registration, groups, machine classes and images, and the native job protocol.
//!
//! # Registration
//!
//! An admin creates a one-hour registration token for a repo or org; a runner exchanges it
//! ([`Ci::register_runner`]) for its identity and a credential: a Biscuit for the principal
//! `runner:<id>` that allows no repo action and is only accepted by the job protocol. A
//! just-in-time config ([`Ci::jit_config`]) registers an ephemeral runner directly.
//!
//! # Job protocol
//!
//! [`Ci::claim`] hands a runner the oldest queued job whose `runs-on` labels the runner has
//! (case-insensitive) and that the runner may serve: repo runners serve their repo; org
//! runners serve repos their group admits (all, private, or selected repos; public repos only
//! when allowed; selected workflows when restricted), and a job naming `runs-on.group` only
//! runners of that group. The claim takes a lease the runner renews with
//! [`Ci::renew`]; renewals report cancellation. A job ends with [`Ci::complete`]. Leases that
//! lapse are handled by the maintenance tick.

use std::collections::BTreeMap;

use bee_auth::{MintRequest, Scope};
use bee_proto::v1::{
    KeyValue, Runner, RunnerArch, RunnerGroup, RunnerGroupVisibility, RunnerImage,
    RunnerImageSource, RunnerJobAssignment, RunnerLabel, RunnerLabelKind, RunnerMachineClass,
    RunnerOs, RunnerScope, RunnerScopeKind, RunnerStatus, SubjectKind, WorkflowConclusion,
    WorkflowRun, WorkflowStatus,
};

use super::engine::{
    Ci, Effects, job_of, job_row, repo_ref_of, run_of, run_row, run_status, status_of,
};
use super::model::{
    GroupRecord, ImageRecord, JobIndex, JobRecord, QueueEntry, RunRecord, RunnerRecord, TokenRecord,
};
use super::util::sha256_hex;
use super::{GROUPS, IMAGES, JOB_INDEX, JOBS, QUEUE, RUNNERS, RUNS, SURFACE, TOKENS};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::store::{Key, Order, Row, Scan};

/// Most custom labels per runner.
pub const MAX_LABELS: usize = 100;

/// Reserved for the server's built-in development runner.
///
/// Only server-internal calls may apply this label. It lets the server replace its own runner
/// records without treating an operator-managed runner with the same display name as stale.
pub const MANAGED_LOCAL_RUNNER_LABEL: &str = "bee-local-runner-managed";
/// Runner credential lifetime.
pub const CREDENTIAL_TTL_MS: u64 = 365 * 86_400_000;

/// Who owns a runner, token or group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunnerOwner {
    /// An org (id).
    Org(String),
    /// A repo (repo id, org id).
    Repo(String, String),
}

impl RunnerOwner {
    /// Storage key (`org:<id>` / `repo:<id>`).
    pub fn key(&self) -> String {
        match self {
            RunnerOwner::Org(o) => format!("org:{o}"),
            RunnerOwner::Repo(r, _) => format!("repo:{r}"),
        }
    }

    /// The org.
    pub fn org_id(&self) -> &str {
        match self {
            RunnerOwner::Org(o) | RunnerOwner::Repo(_, o) => o,
        }
    }

    fn repo_id(&self) -> &str {
        match self {
            RunnerOwner::Org(_) => "",
            RunnerOwner::Repo(r, _) => r,
        }
    }

    fn scope(&self) -> RunnerScope {
        match self {
            RunnerOwner::Org(o) => RunnerScope {
                kind: RunnerScopeKind::Org as i32,
                org: o.clone(),
                repo: None,
            },
            RunnerOwner::Repo(r, o) => RunnerScope {
                kind: RunnerScopeKind::Repo as i32,
                org: o.clone(),
                repo: repo_ref_of(r),
            },
        }
    }
}

/// A runner's row.
pub(crate) fn runner_row(r: &RunnerRecord) -> Row {
    let runner = r.runner.clone().unwrap_or_default();
    let owner = if r.repo_id.is_empty() {
        format!("org:{}", r.org_id)
    } else {
        format!("repo:{}", r.repo_id)
    };
    let managed = runner
        .labels
        .iter()
        .any(|label| label.name.eq_ignore_ascii_case(MANAGED_LOCAL_RUNNER_LABEL));
    let key = if managed {
        format!("{owner}/{}#managed-local", runner.name.to_lowercase())
    } else {
        format!("{owner}/{}", runner.name.to_lowercase())
    };
    Row::new("", runner.id.clone()).k1(key).k2(owner).msg(r)
}

fn default_labels(os: RunnerOs, arch: RunnerArch) -> Vec<RunnerLabel> {
    let mut v = vec!["self-hosted".to_string()];
    v.push(
        match os {
            RunnerOs::Macos => "macOS",
            RunnerOs::Windows => "Windows",
            _ => "Linux",
        }
        .to_string(),
    );
    v.push(
        match arch {
            RunnerArch::Arm64 => "ARM64",
            _ => "X64",
        }
        .to_string(),
    );
    v.into_iter()
        .map(|name| RunnerLabel {
            name,
            kind: RunnerLabelKind::Default as i32,
        })
        .collect()
}

fn custom_labels(labels: &[String], allow_managed_local_runner: bool) -> Result<Vec<RunnerLabel>> {
    if labels.len() > MAX_LABELS {
        return Err(ForgeError::too_large(
            "runner_labels",
            MAX_LABELS as u64,
            labels.len() as u64,
        ));
    }
    let mut out: Vec<RunnerLabel> = Vec::new();
    for l in labels {
        let l = l.trim();
        if l.eq_ignore_ascii_case(MANAGED_LOCAL_RUNNER_LABEL) && !allow_managed_local_runner {
            return Err(ForgeError::invalid(
                "reserved_runner_label",
                "runner label bee-local-runner-managed is reserved",
            ));
        }
        if l.is_empty() || l.len() > 256 || l.contains(',') {
            return Err(ForgeError::invalid(
                "invalid_label",
                format!("bad runner label {l:?}"),
            ));
        }
        if !out.iter().any(|x| x.name.eq_ignore_ascii_case(l)) {
            out.push(RunnerLabel {
                name: l.to_string(),
                kind: RunnerLabelKind::Custom as i32,
            });
        }
    }
    Ok(out)
}

fn valid_runner_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(ForgeError::invalid(
            "invalid_name",
            format!("bad runner name {name:?}"),
        ));
    }
    Ok(())
}

impl Ci {
    fn with_status(&self, mut r: Runner, last_seen: u64) -> Runner {
        let online = last_seen + self.config.runner_offline_ms > self.now();
        r.status = if online {
            RunnerStatus::Online
        } else {
            RunnerStatus::Offline
        } as i32;
        r.last_seen_at = Some(ts(last_seen));
        r
    }

    /// Creates a registration (`kind = "register"`) or removal (`"remove"`) token.
    pub async fn runner_token(
        &self,
        cx: &Cx,
        owner: &RunnerOwner,
        kind: &str,
    ) -> Result<(String, u64)> {
        let token = format!("SWR{}", self.db.ids().random_hex(20)?.to_uppercase());
        let exp = self.now() + self.config.registration_token_ttl_ms;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.insert(
            &TOKENS,
            &Row::new("", sha256_hex(&token))
                .n1(exp as i64)
                .msg(&TokenRecord {
                    kind: kind.into(),
                    org_id: owner.org_id().into(),
                    repo_id: owner.repo_id().into(),
                    expires_ms: exp,
                }),
        )
        .await?;
        tx.audit(
            AuditDraft::new(&format!("runner.{kind}_token_create"), owner.org_id())
                .repo_id(owner.repo_id())
                .target_id(&owner.key()),
        )
        .await?;
        tx.commit().await?;
        Ok((token, exp))
    }

    async fn ensure_default_group(&self, cx: &Cx, org_id: &str) -> Result<RunnerGroup> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if let Some((g, _)) = tx
            .scan_msgs::<GroupRecord>(
                &GROUPS,
                &Scan::scope(org_id, 1).key(Key::K1("default".into())),
            )
            .await?
            .into_iter()
            .next()
        {
            return Ok(g.group.unwrap_or_default());
        }
        let g = RunnerGroup {
            id: tx.new_id()?,
            org: org_id.to_string(),
            name: "Default".into(),
            is_default: true,
            visibility: RunnerGroupVisibility::All as i32,
            allows_public_repos: false,
            created_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        tx.insert(
            &GROUPS,
            &Row::new(org_id, g.id.clone())
                .k1("default")
                .msg(&GroupRecord {
                    group: Some(g.clone()),
                    repo_ids: Vec::new(),
                }),
        )
        .await?;
        tx.audit(AuditDraft::new("runner.group_create", org_id).target_id(&g.id))
            .await?;
        tx.commit().await?;
        Ok(g)
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_runner(
        &self,
        cx: &Cx,
        owner: &RunnerOwner,
        name: &str,
        os: RunnerOs,
        arch: RunnerArch,
        labels: &[String],
        group_id: &str,
        ephemeral: bool,
        version: &str,
    ) -> Result<(Runner, String)> {
        valid_runner_name(name)?;
        let mut all = default_labels(os, arch);
        let allow_managed_local_runner = cx.actor_id == "system" && cx.decision == "system";
        all.extend(custom_labels(labels, allow_managed_local_runner)?);
        let group = match owner {
            RunnerOwner::Repo(..) => String::new(),
            RunnerOwner::Org(o) => {
                if group_id.is_empty() {
                    self.ensure_default_group(cx, o).await?.id
                } else {
                    self.get_group(cx, o, group_id).await?.id
                }
            }
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let key = if all
            .iter()
            .any(|label| label.name.eq_ignore_ascii_case(MANAGED_LOCAL_RUNNER_LABEL))
        {
            format!("{}/{}#managed-local", owner.key(), name.to_lowercase())
        } else {
            format!("{}/{}", owner.key(), name.to_lowercase())
        };
        if !tx
            .scan(
                &RUNNERS,
                &Scan {
                    scope: Some(String::new()),
                    key: Some(Key::K1(key)),
                    limit: 1,
                    ..Scan::default()
                },
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("runner {name}")));
        }
        let now = tx.now_ms();
        let id = tx.new_id()?;
        let (cred, cred_id) = {
            let principal = bee_auth::Principal::service(format!("runner:{id}"), name);
            let t = self.signer.mint(
                &MintRequest {
                    principal,
                    on_behalf_of: Vec::new(),
                    scope: Scope::new()
                        .actions([])
                        .expires_at((now + CREDENTIAL_TTL_MS) / 1000),
                },
                now / 1000,
            )?;
            (t.text, t.id.0)
        };
        let runner = Runner {
            id: id.clone(),
            scope: Some(owner.scope()),
            name: name.to_string(),
            os: os as i32,
            arch: arch as i32,
            status: RunnerStatus::Online as i32,
            busy: false,
            ephemeral,
            hosted: false,
            native: true,
            labels: all,
            runner_group_id: group,
            version: version.to_string(),
            current_job_id: String::new(),
            created_at: Some(ts(now)),
            last_seen_at: Some(ts(now)),
        };
        let rec = RunnerRecord {
            runner: Some(runner.clone()),
            org_id: owner.org_id().into(),
            repo_id: owner.repo_id().into(),
            credential_id: cred_id,
            jobs: Vec::new(),
            last_seen_ms: now,
            spent: false,
        };
        tx.insert(&RUNNERS, &runner_row(&rec)).await?;
        tx.emit_event(bee_proto::v1::Event {
            kind: "runner.registered".into(),
            repo_id: owner.repo_id().into(),
            org_id: owner.org_id().into(),
            payload_json: serde_json::to_string(&runner).unwrap_or_default(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("runner.register", owner.org_id())
                .repo_id(owner.repo_id())
                .target_id(&id)
                .after(&serde_json::json!({ "name": name, "ephemeral": ephemeral }).to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok((runner, cred))
    }

    /// Exchanges a registration token for a runner and its credential.
    #[allow(clippy::too_many_arguments)]
    pub async fn register_runner(
        &self,
        cx: &Cx,
        token: &str,
        name: &str,
        os: RunnerOs,
        arch: RunnerArch,
        labels: &[String],
        group_id: &str,
        ephemeral: bool,
        version: &str,
    ) -> Result<(Runner, String)> {
        let owner = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((t, _)) = tx
                .get_msg::<TokenRecord>(&TOKENS, "", &sha256_hex(token))
                .await?
            else {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::Unauthenticated,
                    "unknown registration token",
                ));
            };
            if t.kind != "register" {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::Unauthenticated,
                    "not a registration token",
                ));
            }
            if t.expires_ms <= tx.now_ms() {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::TokenExpired,
                    "registration token expired",
                ));
            }
            if t.repo_id.is_empty() {
                RunnerOwner::Org(t.org_id)
            } else {
                RunnerOwner::Repo(t.repo_id, t.org_id)
            }
        };
        self.create_runner(
            cx, &owner, name, os, arch, labels, group_id, ephemeral, version,
        )
        .await
    }

    /// Registers an ephemeral runner and returns it with its encoded just-in-time config.
    pub async fn jit_config(
        &self,
        cx: &Cx,
        owner: &RunnerOwner,
        name: &str,
        group_id: &str,
        labels: &[String],
        work_folder: &str,
    ) -> Result<(Runner, String)> {
        let (runner, cred) = self
            .create_runner(
                cx,
                owner,
                name,
                RunnerOs::Linux,
                RunnerArch::X64,
                labels,
                group_id,
                true,
                "",
            )
            .await?;
        let cfg = serde_json::json!({
            "runner_id": runner.id,
            "name": runner.name,
            "credential": cred,
            "server_url": self.config.server_url,
            "work_folder": if work_folder.is_empty() { "_work" } else { work_folder },
            "labels": runner.labels.iter().map(|l| &l.name).collect::<Vec<_>>(),
        });
        Ok((runner, super::util::b64(cfg.to_string().as_bytes())))
    }

    /// A runner and its record.
    pub(crate) async fn runner_record(&self, cx: &Cx, id: &str) -> Result<RunnerRecord> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<RunnerRecord>(&RUNNERS, "", id)
            .await?
            .map(|(r, _)| r)
            .ok_or_else(|| ForgeError::not_found(format!("runner {id}")))
    }

    /// One runner of an owner.
    pub async fn get_runner(&self, cx: &Cx, owner: &RunnerOwner, id: &str) -> Result<Runner> {
        let r = self.runner_record(cx, id).await?;
        let mine = match owner {
            RunnerOwner::Org(o) => r.repo_id.is_empty() && r.org_id == *o,
            RunnerOwner::Repo(repo, _) => r.repo_id == *repo,
        };
        if !mine {
            return Err(ForgeError::not_found(format!("runner {id}")));
        }
        Ok(self.with_status(r.runner.unwrap_or_default(), r.last_seen_ms))
    }

    /// Runners of an owner, filtered.
    pub async fn list_runners(
        &self,
        cx: &Cx,
        owner: &RunnerOwner,
        group: &str,
        label: &str,
        status: RunnerStatus,
        name: &str,
    ) -> Result<Vec<Runner>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<RunnerRecord>(
                &RUNNERS,
                &Scan {
                    scope: Some(String::new()),
                    key: Some(Key::K2(owner.key())),
                    limit: 10_000,
                    ..Scan::default()
                },
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|(r, _)| self.with_status(r.runner.clone().unwrap_or_default(), r.last_seen_ms))
            .filter(|r| group.is_empty() || r.runner_group_id == group)
            .filter(|r| {
                label.is_empty() || r.labels.iter().any(|l| l.name.eq_ignore_ascii_case(label))
            })
            .filter(|r| status == RunnerStatus::Unspecified || r.status == status as i32)
            .filter(|r| name.is_empty() || r.name.eq_ignore_ascii_case(name))
            .collect())
    }

    /// Removes a runner; a busy runner needs `force`, and its jobs fail.
    pub async fn delete_runner(
        &self,
        cx: &Cx,
        owner: &RunnerOwner,
        id: &str,
        force: bool,
    ) -> Result<()> {
        self.get_runner(cx, owner, id).await?;
        let mut fx = Effects::default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((r, _)) = tx.get_msg::<RunnerRecord>(&RUNNERS, "", id).await? else {
            return Err(ForgeError::not_found(format!("runner {id}")));
        };
        if !r.jobs.is_empty() && !force {
            return Err(ForgeError::invalid_state(
                "the runner is running a job; pass force",
            ));
        }
        tx.delete(&RUNNERS, "", id).await?;
        for job_id in &r.jobs {
            let Some((idx, _)) = tx.get_msg::<JobIndex>(&JOB_INDEX, "", job_id).await? else {
                continue;
            };
            if let Some((mut j, row)) = tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id).await?
            {
                j.runner_id.clear();
                j.error = format!(
                    "The runner {} was removed while running the job.",
                    r.runner.as_ref().map(|x| x.name.as_str()).unwrap_or("")
                );
                self.finish_job(
                    &mut tx,
                    &idx.repo_id,
                    &mut j,
                    WorkflowConclusion::Failure,
                    "",
                    &mut fx,
                )
                .await?;
                tx.update(&JOBS, &job_row(&idx.repo_id, &j).version(row.version))
                    .await?;
            }
        }
        tx.emit_event(bee_proto::v1::Event {
            kind: "runner.removed".into(),
            repo_id: r.repo_id.clone(),
            org_id: r.org_id.clone(),
            payload_json: serde_json::to_string(&r.runner).unwrap_or_default(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("runner.delete", &r.org_id)
                .repo_id(&r.repo_id)
                .target_id(id),
        )
        .await?;
        tx.commit().await?;
        self.apply(cx, fx).await;
        Ok(())
    }

    /// Removes a runner with a removal token (the stock runner's `config remove`).
    pub async fn remove_runner_with_token(&self, cx: &Cx, token: &str, id: &str) -> Result<()> {
        let owner = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((t, _)) = tx
                .get_msg::<TokenRecord>(&TOKENS, "", &sha256_hex(token))
                .await?
            else {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::Unauthenticated,
                    "unknown removal token",
                ));
            };
            if t.kind != "remove" || t.expires_ms <= tx.now_ms() {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::TokenExpired,
                    "removal token expired or invalid",
                ));
            }
            if t.repo_id.is_empty() {
                RunnerOwner::Org(t.org_id)
            } else {
                RunnerOwner::Repo(t.repo_id, t.org_id)
            }
        };
        self.delete_runner(cx, &owner, id, true).await
    }

    /// Changes a runner's custom labels: `add`, `remove` one, or `set` all.
    pub async fn edit_labels(
        &self,
        cx: &Cx,
        owner: &RunnerOwner,
        id: &str,
        add: &[String],
        remove: Option<&str>,
        set: Option<&[String]>,
    ) -> Result<Runner> {
        self.get_runner(cx, owner, id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut r, row)) = tx.get_msg::<RunnerRecord>(&RUNNERS, "", id).await? else {
            return Err(ForgeError::not_found(format!("runner {id}")));
        };
        let mut runner = r.runner.clone().unwrap_or_default();
        let mut custom: Vec<String> = runner
            .labels
            .iter()
            .filter(|l| l.kind == RunnerLabelKind::Custom as i32)
            .map(|l| l.name.clone())
            .collect();
        if let Some(s) = set {
            custom = s.to_vec();
        }
        custom.extend(add.iter().cloned());
        if let Some(rm) = remove {
            let before = custom.len();
            custom.retain(|l| !l.eq_ignore_ascii_case(rm));
            if before == custom.len() {
                return Err(ForgeError::not_found(format!("label {rm} on runner {id}")));
            }
        }
        let mut labels: Vec<RunnerLabel> = runner
            .labels
            .iter()
            .filter(|l| l.kind == RunnerLabelKind::Default as i32)
            .cloned()
            .collect();
        labels.extend(custom_labels(&custom, false)?);
        runner.labels = labels;
        r.runner = Some(runner.clone());
        tx.update(&RUNNERS, &runner_row(&r).version(row.version))
            .await?;
        tx.audit(
            AuditDraft::new("runner.labels", &r.org_id)
                .repo_id(&r.repo_id)
                .target_id(id)
                .after(&serde_json::json!({ "labels": custom }).to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(self.with_status(runner, r.last_seen_ms))
    }

    // ----- groups -----------------------------------------------------------------------------

    /// Creates a runner group.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_group(
        &self,
        cx: &Cx,
        org_id: &str,
        name: &str,
        visibility: RunnerGroupVisibility,
        repo_ids: Vec<String>,
        runner_ids: Vec<String>,
        allows_public: bool,
        restricted: bool,
        workflows: Vec<String>,
    ) -> Result<RunnerGroup> {
        if name.trim().is_empty() || name.len() > 100 || name.eq_ignore_ascii_case("default") {
            return Err(ForgeError::invalid(
                "invalid_name",
                format!("bad runner group name {name:?}"),
            ));
        }
        self.ensure_default_group(cx, org_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let lower = name.to_lowercase();
        if !tx
            .scan(&GROUPS, &Scan::scope(org_id, 1).key(Key::K1(lower.clone())))
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("runner group {name}")));
        }
        let g = RunnerGroup {
            id: tx.new_id()?,
            org: org_id.to_string(),
            name: name.to_string(),
            is_default: false,
            visibility: if visibility == RunnerGroupVisibility::Unspecified {
                RunnerGroupVisibility::All
            } else {
                visibility
            } as i32,
            allows_public_repos: allows_public,
            restricted_to_workflows: restricted,
            selected_workflows: workflows,
            network_configuration_id: String::new(),
            runner_count: 0,
            created_at: Some(ts(tx.now_ms())),
        };
        tx.insert(
            &GROUPS,
            &Row::new(org_id, g.id.clone()).k1(lower).msg(&GroupRecord {
                group: Some(g.clone()),
                repo_ids,
            }),
        )
        .await?;
        tx.audit(AuditDraft::new("runner.group_create", org_id).target_id(&g.id))
            .await?;
        tx.commit().await?;
        if !runner_ids.is_empty() {
            self.set_group_runners(cx, org_id, &g.id, runner_ids)
                .await?;
        }
        self.get_group(cx, org_id, &g.id).await
    }

    async fn group_record(&self, cx: &Cx, org_id: &str, id: &str) -> Result<(GroupRecord, Row)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<GroupRecord>(&GROUPS, org_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("runner group {id}")))
    }

    /// One group with its runner count.
    pub async fn get_group(&self, cx: &Cx, org_id: &str, id: &str) -> Result<RunnerGroup> {
        let (g, _) = self.group_record(cx, org_id, id).await?;
        let mut g = g.group.unwrap_or_default();
        g.runner_count = self
            .list_runners(
                cx,
                &RunnerOwner::Org(org_id.into()),
                &g.id,
                "",
                RunnerStatus::Unspecified,
                "",
            )
            .await?
            .len() as u32;
        Ok(g)
    }

    /// An org's groups, optionally only those `visible_to` (a repo) may use.
    pub async fn list_groups(
        &self,
        cx: &Cx,
        org_id: &str,
        visible_to: Option<&bee_proto::v1::Repo>,
    ) -> Result<Vec<RunnerGroup>> {
        self.ensure_default_group(cx, org_id).await?;
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<GroupRecord>(&GROUPS, &Scan::scope(org_id, 1000))
                .await?
        };
        let mut out = Vec::new();
        for (g, _) in rows {
            if let Some(repo) = visible_to {
                let public = repo.visibility == bee_proto::v1::Visibility::Public as i32;
                if !group_admits(&g, &repo.id, public, "") {
                    continue;
                }
            }
            out.push(
                self.get_group(cx, org_id, &g.group.unwrap_or_default().id)
                    .await?,
            );
        }
        Ok(out)
    }

    /// Updates a group's access rules.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_group(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
        name: Option<String>,
        visibility: Option<RunnerGroupVisibility>,
        allows_public: Option<bool>,
        restricted: Option<bool>,
        workflows: Option<Vec<String>>,
        network: Option<String>,
    ) -> Result<RunnerGroup> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, row)) = tx.get_msg::<GroupRecord>(&GROUPS, org_id, id).await? else {
            return Err(ForgeError::not_found(format!("runner group {id}")));
        };
        let mut g = rec.group.clone().unwrap_or_default();
        let mut k1 = row.k1.clone().unwrap_or_default();
        if let Some(n) = name {
            if g.is_default {
                return Err(ForgeError::invalid_state(
                    "the default group cannot be renamed",
                ));
            }
            if n.trim().is_empty() || n.eq_ignore_ascii_case("default") {
                return Err(ForgeError::invalid("invalid_name", "bad runner group name"));
            }
            k1 = n.to_lowercase();
            g.name = n;
        }
        if let Some(v) = visibility {
            g.visibility = v as i32;
        }
        if let Some(v) = allows_public {
            g.allows_public_repos = v;
        }
        if let Some(v) = restricted {
            g.restricted_to_workflows = v;
        }
        if let Some(v) = workflows {
            g.selected_workflows = v;
        }
        if let Some(v) = network {
            g.network_configuration_id = v;
        }
        rec.group = Some(g);
        tx.update(
            &GROUPS,
            &Row::new(org_id, id).k1(k1).version(row.version).msg(&rec),
        )
        .await?;
        tx.audit(AuditDraft::new("runner.group_update", org_id).target_id(id))
            .await?;
        tx.commit().await?;
        self.get_group(cx, org_id, id).await
    }

    /// Deletes a group; its runners move to the default group.
    pub async fn delete_group(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        let (g, _) = self.group_record(cx, org_id, id).await?;
        if g.group.as_ref().is_some_and(|g| g.is_default) {
            return Err(ForgeError::invalid_state(
                "the default group cannot be deleted",
            ));
        }
        let default = self.ensure_default_group(cx, org_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for (mut r, row) in tx
            .scan_msgs::<RunnerRecord>(
                &RUNNERS,
                &Scan {
                    scope: Some(String::new()),
                    key: Some(Key::K2(format!("org:{org_id}"))),
                    limit: 10_000,
                    ..Scan::default()
                },
            )
            .await?
        {
            let mut runner = r.runner.clone().unwrap_or_default();
            if runner.runner_group_id == id {
                runner.runner_group_id = default.id.clone();
                r.runner = Some(runner);
                tx.update(&RUNNERS, &runner_row(&r).version(row.version))
                    .await?;
            }
        }
        tx.delete(&GROUPS, org_id, id).await?;
        tx.audit(AuditDraft::new("runner.group_delete", org_id).target_id(id))
            .await?;
        tx.commit().await
    }

    /// Repos allowed to use a SELECTED group.
    pub async fn group_repos(&self, cx: &Cx, org_id: &str, id: &str) -> Result<Vec<String>> {
        Ok(self.group_record(cx, org_id, id).await?.0.repo_ids)
    }

    /// Replaces the repos allowed to use a group.
    pub async fn set_group_repos(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
        repo_ids: Vec<String>,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, row)) = tx.get_msg::<GroupRecord>(&GROUPS, org_id, id).await? else {
            return Err(ForgeError::not_found(format!("runner group {id}")));
        };
        if rec.group.as_ref().map(|g| g.visibility) != Some(RunnerGroupVisibility::Selected as i32)
        {
            return Err(ForgeError::invalid_state(
                "the group's visibility is not SELECTED",
            ));
        }
        let mut ids = repo_ids;
        ids.sort();
        ids.dedup();
        rec.repo_ids = ids;
        tx.update(
            &GROUPS,
            &Row::new(org_id, id)
                .k1(row.k1.clone().unwrap_or_default())
                .version(row.version)
                .msg(&rec),
        )
        .await?;
        tx.audit(AuditDraft::new("runner.group_repos", org_id).target_id(id))
            .await?;
        tx.commit().await
    }

    /// Replaces the runners in a group (others move to the default group).
    pub async fn set_group_runners(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
        runner_ids: Vec<String>,
    ) -> Result<()> {
        self.group_record(cx, org_id, id).await?;
        let default = self.ensure_default_group(cx, org_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for rid in &runner_ids {
            match tx.get_msg::<RunnerRecord>(&RUNNERS, "", rid).await? {
                Some((r, _)) if r.repo_id.is_empty() && r.org_id == org_id => {}
                _ => return Err(ForgeError::not_found(format!("runner {rid}"))),
            }
        }
        for (mut r, row) in tx
            .scan_msgs::<RunnerRecord>(
                &RUNNERS,
                &Scan {
                    scope: Some(String::new()),
                    key: Some(Key::K2(format!("org:{org_id}"))),
                    limit: 10_000,
                    ..Scan::default()
                },
            )
            .await?
        {
            let mut runner = r.runner.clone().unwrap_or_default();
            let want = if runner_ids.contains(&runner.id) {
                id.to_string()
            } else if runner.runner_group_id == id {
                default.id.clone()
            } else {
                continue;
            };
            if runner.runner_group_id != want {
                runner.runner_group_id = want;
                r.runner = Some(runner);
                tx.update(&RUNNERS, &runner_row(&r).version(row.version))
                    .await?;
            }
        }
        tx.audit(AuditDraft::new("runner.group_runners", org_id).target_id(id))
            .await?;
        tx.commit().await
    }

    // ----- hosted classes and images ----------------------------------------------------------

    /// Hosted machine classes, optionally of one OS.
    pub fn machine_classes(&self, os: RunnerOs) -> Vec<RunnerMachineClass> {
        self.config
            .machine_classes
            .iter()
            .filter(|c| os == RunnerOs::Unspecified || c.os == os as i32)
            .cloned()
            .collect()
    }

    /// Platform images plus the org's custom images.
    pub async fn runner_images(
        &self,
        cx: &Cx,
        org_id: &str,
        source: RunnerImageSource,
    ) -> Result<Vec<RunnerImage>> {
        let mut out = Vec::new();
        if source != RunnerImageSource::Custom {
            out.extend(self.config.platform_images.iter().cloned());
        }
        if source != RunnerImageSource::Platform {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            out.extend(
                tx.scan_msgs::<ImageRecord>(&IMAGES, &Scan::scope(org_id, 1000))
                    .await?
                    .into_iter()
                    .filter_map(|(r, _)| r.image),
            );
        }
        Ok(out)
    }

    /// Registers a custom image (the OCI reference must be pinned by digest).
    pub async fn create_image(
        &self,
        cx: &Cx,
        org_id: &str,
        name: &str,
        os: RunnerOs,
        arch: RunnerArch,
        oci: &str,
    ) -> Result<RunnerImage> {
        if name.is_empty() || name.len() > 100 {
            return Err(ForgeError::invalid("invalid_name", "bad image name"));
        }
        let pinned = oci.rsplit_once("@sha256:").is_some_and(|(r, d)| {
            !r.is_empty() && d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())
        });
        if !pinned {
            return Err(ForgeError::invalid(
                "unpinned_image",
                "the OCI reference must be pinned by digest (`@sha256:<hex>`)",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let img = RunnerImage {
            id: tx.new_id()?,
            name: name.to_string(),
            source: RunnerImageSource::Custom as i32,
            os: os as i32,
            arch: arch as i32,
            oci_reference: oci.to_string(),
            size_bytes: 0,
            created_at: Some(ts(tx.now_ms())),
        };
        if !tx
            .scan(
                &IMAGES,
                &Scan::scope(org_id, 1).key(Key::K1(name.to_lowercase())),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("runner image {name}")));
        }
        tx.insert(
            &IMAGES,
            &Row::new(org_id, img.id.clone())
                .k1(name.to_lowercase())
                .msg(&ImageRecord {
                    image: Some(img.clone()),
                }),
        )
        .await?;
        tx.audit(AuditDraft::new("runner.image_create", org_id).target_id(&img.id))
            .await?;
        tx.commit().await?;
        Ok(img)
    }

    /// Deletes a custom image.
    pub async fn delete_image(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&IMAGES, org_id, id).await? {
            return Err(ForgeError::not_found(format!("runner image {id}")));
        }
        tx.audit(AuditDraft::new("runner.image_delete", org_id).target_id(id))
            .await?;
        tx.commit().await
    }

    // ----- job protocol -----------------------------------------------------------------------

    /// Checks that `caller` (a principal id) is runner `runner_id` holding the given credential.
    pub(crate) async fn authenticate_runner(
        &self,
        cx: &Cx,
        caller: &str,
        token_ids: &[String],
        runner_id: &str,
    ) -> Result<RunnerRecord> {
        if caller != format!("runner:{runner_id}") {
            return Err(ForgeError::permission_denied(
                "the credential is not this runner's",
            ));
        }
        let r = self.runner_record(cx, runner_id).await?;
        if !token_ids.contains(&r.credential_id) {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::TokenRevoked,
                "the runner credential was replaced",
            ));
        }
        Ok(r)
    }

    /// Takes the oldest queued job runner `runner_id` may serve, if any.
    pub async fn claim(
        &self,
        cx: &Cx,
        runner_id: &str,
        offered: &[String],
        max_jobs: u32,
    ) -> Result<Option<RunnerJobAssignment>> {
        let candidates = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<QueueEntry>(
                &QUEUE,
                &Scan {
                    order: Order::N1,
                    limit: 500,
                    ..Scan::default()
                },
            )
            .await?
        };
        let max = max_jobs.max(1) as usize;
        for (q, qrow) in candidates {
            let mut fx = Effects::default();
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let Some((mut r, rrow)) = tx.get_msg::<RunnerRecord>(&RUNNERS, "", runner_id).await?
            else {
                return Err(ForgeError::not_found(format!("runner {runner_id}")));
            };
            let now = tx.now_ms();
            r.last_seen_ms = now;
            let mut runner = r.runner.clone().unwrap_or_default();
            if r.jobs.len() >= max || (runner.ephemeral && r.spent) {
                tx.update(&RUNNERS, &runner_row(&r).version(rrow.version))
                    .await?;
                tx.commit().await?;
                return Ok(None);
            }
            let labels: Vec<String> = if offered.is_empty() {
                runner.labels.iter().map(|l| l.name.clone()).collect()
            } else {
                offered.to_vec()
            };
            if !q
                .labels
                .iter()
                .all(|want| labels.iter().any(|l| l.eq_ignore_ascii_case(want)))
            {
                continue;
            }
            if !r.repo_id.is_empty() {
                if r.repo_id != q.repo_id || !q.group.is_empty() {
                    continue;
                }
            } else {
                if r.org_id != q.org_id {
                    continue;
                }
                let Some((g, _)) = tx
                    .get_msg::<GroupRecord>(&GROUPS, &r.org_id, &runner.runner_group_id)
                    .await?
                else {
                    continue;
                };
                let gname = g.group.as_ref().map(|g| g.name.clone()).unwrap_or_default();
                if !q.group.is_empty() && !gname.eq_ignore_ascii_case(&q.group) {
                    continue;
                }
                if !group_admits(&g, &q.repo_id, q.public, &q.workflow_ref) {
                    continue;
                }
            }
            let Some((mut j, jrow)) = tx.get_msg::<JobRecord>(&JOBS, &q.repo_id, &qrow.id).await?
            else {
                tx.delete(&QUEUE, "", &qrow.id).await?;
                tx.commit().await?;
                continue;
            };
            if status_of(job_of(&j)) != WorkflowStatus::Queued {
                tx.delete(&QUEUE, "", &qrow.id).await?;
                tx.commit().await?;
                continue;
            }
            let Some((mut rec, run_row_v)) = tx
                .get_msg::<RunRecord>(&RUNS, &q.repo_id, &job_of(&j).run_id)
                .await?
            else {
                continue;
            };
            let lease_id = tx.new_id()?;
            let lease_exp = now + self.config.lease_ms;
            j.lease_id = lease_id.clone();
            j.lease_expires_ms = lease_exp;
            j.runner_id = runner_id.to_string();
            j.deadline_ms = if j.timeout_ms > 0 {
                now + j.timeout_ms
            } else {
                0
            };
            j.secrets_nonce = self.db.ids().random_hex(16)?;
            j.secrets_redeemed = false;
            let job_id = job_of(&j).id.clone();
            let token_exp = if j.deadline_ms > 0 {
                j.deadline_ms + 600_000
            } else {
                now + 6 * 3_600_000
            };
            let (token, token_id) =
                self.mint_job_token(&job_id, &q.repo_id, &j.permissions, token_exp)?;
            j.token_id = token_id;
            let mut job = job_of(&j).clone();
            job.status = WorkflowStatus::InProgress as i32;
            job.started_at = Some(ts(now));
            job.runner_id = runner_id.to_string();
            job.runner_name = runner.name.clone();
            job.runner_group_id = runner.runner_group_id.clone();
            j.job = Some(job.clone());
            tx.update(&JOBS, &job_row(&q.repo_id, &j).version(jrow.version))
                .await?;
            tx.delete(&QUEUE, "", &job_id).await?;
            let attention = [j.lease_expires_ms, j.deadline_ms]
                .into_iter()
                .filter(|t| *t > 0)
                .min()
                .unwrap_or(lease_exp);
            tx.put(
                &JOB_INDEX,
                &Row::new("", job_id.clone())
                    .n1(attention as i64)
                    .msg(&JobIndex {
                        repo_id: q.repo_id.clone(),
                        org_id: q.org_id.clone(),
                        run_id: job.run_id.clone(),
                    }),
            )
            .await?;
            r.jobs.push(job_id.clone());
            if runner.ephemeral {
                r.spent = true;
            }
            runner.busy = true;
            runner.current_job_id = job_id.clone();
            runner.status = RunnerStatus::Online as i32;
            r.runner = Some(runner.clone());
            tx.update(&RUNNERS, &runner_row(&r).version(rrow.version))
                .await?;
            let run: WorkflowRun = run_of(&rec).clone();
            if run_status(&run) == WorkflowStatus::Queued {
                let mut run2 = run.clone();
                run2.status = WorkflowStatus::InProgress as i32;
                run2.updated_at = Some(ts(now));
                rec.run = Some(run2);
                tx.update(&RUNS, &run_row(&q.repo_id, &rec).version(run_row_v.version))
                    .await?;
                self.emit_run(&mut tx, &rec, "workflow_run.in_progress")
                    .await?;
            }
            if !j.deployment_id.is_empty() {
                self.deployment_progress(
                    &mut tx,
                    &q.repo_id,
                    &j.deployment_id,
                    bee_proto::v1::DeploymentState::InProgress,
                    &q.org_id,
                )
                .await?;
            }
            Self::emit_job(&mut tx, &q.repo_id, &j, "workflow_job.in_progress").await?;
            tx.audit(
                AuditDraft::new("runner.job_claim", &q.org_id)
                    .repo_id(&q.repo_id)
                    .target(SubjectKind::WorkflowRun, &job.run_id)
                    .after(&serde_json::json!({ "job": job_id, "runner": runner_id }).to_string()),
            )
            .await?;
            tx.commit().await?;
            fx.sync.insert((q.repo_id.clone(), job.run_id.clone()));
            self.apply(cx, fx).await;
            let secrets_url = self.urls.sign(
                &format!("/_ci/jobs/{job_id}/secrets/{}", j.secrets_nonce),
                now + self.config.signed_url_ttl_ms,
            );
            return Ok(Some(RunnerJobAssignment {
                job_id: job_id.clone(),
                run_id: job.run_id.clone(),
                repo: repo_ref_of(&q.repo_id),
                lease_id,
                lease_expires_at: Some(ts(lease_exp)),
                job_token: token,
                plan_json: j.plan_json.clone(),
                head_changeset: job.head_changeset.clone(),
                head_ref: job.head_ref.clone(),
                timeout_seconds: (j.timeout_ms / 1000) as u32,
                secrets_url,
            }));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if let Some((mut r, row)) = tx.get_msg::<RunnerRecord>(&RUNNERS, "", runner_id).await? {
            r.last_seen_ms = tx.now_ms();
            tx.update(&RUNNERS, &runner_row(&r).version(row.version))
                .await?;
            tx.commit().await?;
        }
        Ok(None)
    }

    async fn leased(
        &self,
        cx: &Cx,
        runner_id: &str,
        job_id: &str,
        lease_id: &str,
    ) -> Result<(JobIndex, JobRecord)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let lost = || {
            ForgeError::invalid_state("the job lease was lost; stop the job")
                .with_subcode("lease_lost")
        };
        let Some((idx, _)) = tx.get_msg::<JobIndex>(&JOB_INDEX, "", job_id).await? else {
            return Err(lost());
        };
        let Some((j, _)) = tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id).await? else {
            return Err(lost());
        };
        if j.runner_id != runner_id
            || j.lease_id != lease_id
            || status_of(job_of(&j)) != WorkflowStatus::InProgress
        {
            return Err(lost());
        }
        Ok((idx, j))
    }

    /// Extends a job lease; reports cancellation. `INVALID_STATE` when the lease is lost.
    pub async fn renew(
        &self,
        cx: &Cx,
        runner_id: &str,
        job_id: &str,
        lease_id: &str,
    ) -> Result<(u64, bool, bool)> {
        let (idx, _) = self.leased(cx, runner_id, job_id, lease_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut j, row)) = tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id).await? else {
            return Err(ForgeError::invalid_state("the job lease was lost"));
        };
        let now = tx.now_ms();
        j.lease_expires_ms = now + self.config.lease_ms;
        let attention = [j.lease_expires_ms, j.deadline_ms]
            .into_iter()
            .filter(|t| *t > 0)
            .min()
            .unwrap_or(j.lease_expires_ms);
        tx.update(&JOBS, &job_row(&idx.repo_id, &j).version(row.version))
            .await?;
        tx.put(
            &JOB_INDEX,
            &Row::new("", job_id).n1(attention as i64).msg(&idx),
        )
        .await?;
        if let Some((mut r, rrow)) = tx.get_msg::<RunnerRecord>(&RUNNERS, "", runner_id).await? {
            r.last_seen_ms = now;
            tx.update(&RUNNERS, &runner_row(&r).version(rrow.version))
                .await?;
        }
        tx.commit().await?;
        Ok((j.lease_expires_ms, j.cancel_requested, j.force_cancel))
    }

    /// Records a step's status change.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_step(
        &self,
        cx: &Cx,
        runner_id: &str,
        job_id: &str,
        lease_id: &str,
        number: u32,
        name: &str,
        status: WorkflowStatus,
        conclusion: WorkflowConclusion,
        started_ms: u64,
        completed_ms: u64,
    ) -> Result<()> {
        if number == 0 {
            return Err(ForgeError::invalid(
                "invalid_field",
                "step numbers start at 1",
            ));
        }
        let (idx, _) = self.leased(cx, runner_id, job_id, lease_id).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut j, row)) = tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id).await? else {
            return Err(ForgeError::invalid_state("the job lease was lost"));
        };
        let mut job = job_of(&j).clone();
        let offset = job.log_size_bytes;
        let pos = match job.steps.iter().position(|s| s.number == number) {
            Some(p) => p,
            None => {
                job.steps.push(bee_proto::v1::WorkflowStep {
                    number,
                    ..Default::default()
                });
                job.steps.sort_by_key(|s| s.number);
                job.steps
                    .iter()
                    .position(|s| s.number == number)
                    .unwrap_or(0)
            }
        };
        let s = &mut job.steps[pos];
        if !name.is_empty() {
            s.name = name.to_string();
        }
        if status != WorkflowStatus::Unspecified {
            if status == WorkflowStatus::InProgress && s.status != WorkflowStatus::InProgress as i32
            {
                s.log_offset = offset;
            }
            s.status = status as i32;
        }
        if conclusion != WorkflowConclusion::Unspecified {
            s.conclusion = conclusion as i32;
        }
        if started_ms > 0 {
            s.started_at = Some(ts(started_ms));
        }
        if completed_ms > 0 {
            s.completed_at = Some(ts(completed_ms));
        }
        j.job = Some(job);
        tx.update(&JOBS, &job_row(&idx.repo_id, &j).version(row.version))
            .await?;
        tx.commit().await
    }

    /// Completes a job with the runner's conclusion, outputs and step summary.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete(
        &self,
        cx: &Cx,
        runner_id: &str,
        job_id: &str,
        lease_id: &str,
        conclusion: WorkflowConclusion,
        outputs: Vec<KeyValue>,
        summary: &str,
        environment_url: &str,
    ) -> Result<()> {
        if matches!(conclusion, WorkflowConclusion::Unspecified) {
            return Err(ForgeError::missing("conclusion"));
        }
        let (idx, leased) = self.leased(cx, runner_id, job_id, lease_id).await?;
        let repo = self.repo_by_id(cx, &idx.repo_id).await?;
        let secrets: Vec<String> = self
            .secrets_for(cx, &repo, &leased.environment)
            .await?
            .into_values()
            .collect();
        let mut fx = Effects::default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut j, row)) = tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id).await? else {
            return Err(ForgeError::invalid_state("the job lease was lost"));
        };
        let mut outs: BTreeMap<String, String> = BTreeMap::new();
        for kv in outputs {
            outs.insert(kv.key, kv.value);
        }
        j.outputs = super::convert::to_kv(
            &outs
                .into_iter()
                .map(|(k, v)| {
                    // GitHub drops outputs that contain secrets.
                    let masked = super::util::mask(&v, &secrets);
                    (k, if masked != v { String::new() } else { v })
                })
                .collect(),
        );
        j.summary = super::util::mask(&truncate(summary, 1024 * 1024), &secrets);
        j.result = super::convert::result_str(conclusion).into();
        j.environment_url = truncate(environment_url.trim(), 2048);
        self.finish_job(&mut tx, &idx.repo_id, &mut j, conclusion, "", &mut fx)
            .await?;
        tx.update(&JOBS, &job_row(&idx.repo_id, &j).version(row.version))
            .await?;
        tx.audit(
            AuditDraft::new("runner.job_complete", &idx.org_id)
                .repo_id(&idx.repo_id)
                .target(SubjectKind::WorkflowRun, &idx.run_id)
                .after(
                    &serde_json::json!({ "job": job_id, "conclusion": conclusion.as_str_name() })
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        self.apply(cx, fx).await;
        Ok(())
    }

    /// The leased job's secrets, once (`RunnerService.FetchRunnerJobSecrets`): the RPC form of
    /// the `secrets_url` grant, so it is spent by either.
    pub async fn fetch_job_secrets(
        &self,
        cx: &Cx,
        runner_id: &str,
        job_id: &str,
        lease_id: &str,
    ) -> Result<BTreeMap<String, String>> {
        let (idx, _) = self.leased(cx, runner_id, job_id, lease_id).await?;
        let nonce = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id)
                .await?
                .map(|(j, _)| j.secrets_nonce)
                .unwrap_or_default()
        };
        self.redeem_secrets(cx, job_id, &nonce).await
    }

    /// Hands a running job its secrets once (the server's `secrets_url` handler).
    pub async fn redeem_secrets(
        &self,
        cx: &Cx,
        job_id: &str,
        nonce: &str,
    ) -> Result<BTreeMap<String, String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let denied = || ForgeError::permission_denied("the secrets grant is not valid");
        let Some((idx, _)) = tx.get_msg::<JobIndex>(&JOB_INDEX, "", job_id).await? else {
            return Err(denied());
        };
        let Some((mut j, row)) = tx.get_msg::<JobRecord>(&JOBS, &idx.repo_id, job_id).await? else {
            return Err(denied());
        };
        if j.secrets_redeemed
            || j.secrets_nonce.is_empty()
            || j.secrets_nonce != nonce
            || status_of(job_of(&j)) != WorkflowStatus::InProgress
        {
            return Err(denied());
        }
        j.secrets_redeemed = true;
        tx.update(&JOBS, &job_row(&idx.repo_id, &j).version(row.version))
            .await?;
        tx.audit(
            AuditDraft::new("secret.job_redeem", &idx.org_id)
                .repo_id(&idx.repo_id)
                .target(SubjectKind::WorkflowRun, &idx.run_id)
                .after(&serde_json::json!({ "job": job_id }).to_string()),
        )
        .await?;
        tx.commit().await?;
        let repo = self.repo_by_id(cx, &idx.repo_id).await?;
        let run = self.run_record(cx, &idx.repo_id, &idx.run_id).await?;
        if run.from_fork && !self.effective_policy(cx, &repo).await?.fork_send_secrets {
            // Fork change requests run untrusted code: no secrets unless the policy says so.
            return Ok(BTreeMap::new());
        }
        let mut all = self.secrets_for(cx, &repo, &j.environment).await?;
        if let Some(scope) = run.scopes.iter().find(|s| s.id == j.scope)
            && !scope.secrets_inherit
        {
            // A reusable workflow sees only the secrets its caller passed.
            let ctx = bee_workflow::expr::MapContext::default().with(
                "secrets",
                bee_workflow::Value::from(
                    all.iter()
                        .map(|(k, v)| (k.clone(), bee_workflow::Value::from(v.as_str())))
                        .collect::<bee_workflow::value::Object>(),
                ),
            );
            let mut passed = BTreeMap::new();
            for kv in &scope.secrets {
                let t = serde_json::from_str(&kv.value)
                    .ok()
                    .and_then(|v| super::convert::template_from_json(&v));
                if let Some(t) = t {
                    passed.insert(
                        kv.key.to_ascii_uppercase(),
                        t.evaluate_string(&ctx).unwrap_or_default(),
                    );
                }
            }
            all = passed;
        }
        let _ = ts_ms;
        Ok(all)
    }

    /// Removes expired registration and removal tokens.
    pub(crate) async fn expire_tokens(&self, cx: &Cx) -> Result<()> {
        let now = self.now();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &TOKENS,
                &Scan {
                    order: Order::N1,
                    n1_max: Some(now as i64),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        for r in rows {
            tx.delete(&TOKENS, "", &r.id).await?;
        }
        tx.commit().await
    }
}

/// Whether a group admits a job of `repo_id` (public or not) from `workflow`.
fn group_admits(g: &GroupRecord, repo_id: &str, public: bool, workflow: &str) -> bool {
    let group = g.group.clone().unwrap_or_default();
    if public && !group.allows_public_repos {
        return false;
    }
    let ok = match RunnerGroupVisibility::try_from(group.visibility)
        .unwrap_or(RunnerGroupVisibility::All)
    {
        RunnerGroupVisibility::Selected => g.repo_ids.iter().any(|r| r == repo_id),
        RunnerGroupVisibility::Private => !public,
        _ => true,
    };
    if !ok {
        return false;
    }
    if group.restricted_to_workflows && !workflow.is_empty() {
        return group
            .selected_workflows
            .iter()
            .any(|w| w.split('@').next().unwrap_or(w).ends_with(workflow));
    }
    true
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
