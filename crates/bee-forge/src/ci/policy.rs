//! Actions policies, required workflows, OIDC tokens, attestations, usage and status badges.
//!
//! # Policies
//!
//! Values resolve platform default → org → repo. A repo may tighten but never loosen what its
//! org sets (`POLICY_DENIED`); fields the org sets that a repo cannot override are reported
//! in `org_enforced`. Runs check `allowed_actions` and `sha_pinning_required` against every
//! `uses:` of the plan before any job starts (a violation is a `startup_failure`).

use std::collections::BTreeMap;

use bee_proto::v1::{
    ActionsPolicy, ActionsPolicyAllowedActions, ActionsPolicyEnabledRepos,
    ActionsPolicyForkApproval, ActionsPolicyScope, ActionsPolicyScopeKind,
    ActionsPolicyTokenPermission, Attestation, AttestationSubject, Repo, SetActionsPolicyRequest,
    WorkflowJobUsage, WorkflowOidcClaimTemplate, WorkflowRequirement, WorkflowRequirementScope,
    WorkflowRunUsage, WorkflowRunnerUsage, WorkflowUsage,
};
use bee_workflow::plan::{CalledWorkflow, JobBody, PlannedJob, PlannedRun, PlannedStepKind};
use serde_json::json;

use super::engine::{Ci, job_of, repo_ref_of, run_of};
use super::model::{
    AttestationRecord, JobRecord, PolicyRecord, RequiredRecord, RunRecord, UsageRecord,
};
use super::util::{b64, b64url};
use super::{ATTESTATIONS, POLICIES, REQUIRED, RUNS, SURFACE, USAGE, WORKFLOWS};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Key, Row, Scan};

/// Where a policy lives.
#[derive(Clone, Debug, PartialEq)]
pub enum PolicyScope {
    /// An org (by id).
    Org(String),
    /// A repo.
    Repo(Box<Repo>),
}

impl PolicyScope {
    fn key(&self) -> String {
        match self {
            PolicyScope::Org(o) => format!("org:{o}"),
            PolicyScope::Repo(r) => format!("repo:{}", r.id),
        }
    }

    fn proto(&self) -> ActionsPolicyScope {
        match self {
            PolicyScope::Org(o) => ActionsPolicyScope {
                kind: ActionsPolicyScopeKind::Org as i32,
                org: o.clone(),
                repo: None,
            },
            PolicyScope::Repo(r) => ActionsPolicyScope {
                kind: ActionsPolicyScopeKind::Repo as i32,
                org: r.org_id.clone(),
                repo: repo_ref_of(&r.id),
            },
        }
    }
}

/// The platform default policy.
pub fn default_policy(c: &super::CiConfig) -> ActionsPolicy {
    ActionsPolicy {
        enabled: true,
        enabled_repos: ActionsPolicyEnabledRepos::All as i32,
        allowed_actions: ActionsPolicyAllowedActions::All as i32,
        fork_approval: ActionsPolicyForkApproval::FirstTimeContributors as i32,
        default_token_permission: ActionsPolicyTokenPermission::Read as i32,
        artifact_retention_days: c.artifact_retention_days,
        log_retention_days: c.log_retention_days,
        cache_max_size_bytes: c.cache_max_bytes,
        cache_retention_days: c.cache_retention_days,
        ..Default::default()
    }
}

fn strictness(a: i32) -> u8 {
    match ActionsPolicyAllowedActions::try_from(a).unwrap_or(ActionsPolicyAllowedActions::All) {
        ActionsPolicyAllowedActions::LocalOnly => 3,
        ActionsPolicyAllowedActions::Selected => 2,
        _ => 1,
    }
}

/// Applies the set fields of `req` onto `p`; returns the names set.
fn apply_set(p: &mut ActionsPolicy, req: &SetActionsPolicyRequest) -> Vec<&'static str> {
    let mut set = Vec::new();
    macro_rules! f {
        ($field:ident, $name:literal) => {
            if let Some(v) = req.$field {
                p.$field = v;
                set.push($name);
            }
        };
    }
    f!(enabled, "enabled");
    f!(enabled_repos, "enabled_repos");
    f!(allowed_actions, "allowed_actions");
    f!(verified_creators_allowed, "verified_creators_allowed");
    f!(sha_pinning_required, "sha_pinning_required");
    f!(fork_approval, "fork_approval");
    f!(agent_runs_require_approval, "agent_runs_require_approval");
    f!(fork_runs_on_private, "fork_runs_on_private");
    f!(fork_send_write_tokens, "fork_send_write_tokens");
    f!(fork_send_secrets, "fork_send_secrets");
    f!(
        fork_require_approval_private,
        "fork_require_approval_private"
    );
    f!(default_token_permission, "default_token_permission");
    f!(
        token_can_approve_change_requests,
        "token_can_approve_change_requests"
    );
    f!(artifact_retention_days, "artifact_retention_days");
    f!(log_retention_days, "log_retention_days");
    f!(cache_max_size_bytes, "cache_max_size_bytes");
    f!(cache_retention_days, "cache_retention_days");
    if let Some(pats) = &req.allowed_patterns {
        p.allowed_patterns = pats.patterns.clone();
        set.push("allowed_patterns");
    }
    set
}

/// Overlays the explicitly set fields of `from` onto `p`.
fn overlay(p: &mut ActionsPolicy, from: &ActionsPolicy, set: &[String]) {
    for s in set {
        match s.as_str() {
            "enabled" => p.enabled = from.enabled,
            "enabled_repos" => p.enabled_repos = from.enabled_repos,
            "allowed_actions" => p.allowed_actions = from.allowed_actions,
            "verified_creators_allowed" => {
                p.verified_creators_allowed = from.verified_creators_allowed
            }
            "sha_pinning_required" => p.sha_pinning_required = from.sha_pinning_required,
            "fork_approval" => p.fork_approval = from.fork_approval,
            "agent_runs_require_approval" => {
                p.agent_runs_require_approval = from.agent_runs_require_approval
            }
            "fork_runs_on_private" => p.fork_runs_on_private = from.fork_runs_on_private,
            "fork_send_write_tokens" => p.fork_send_write_tokens = from.fork_send_write_tokens,
            "fork_send_secrets" => p.fork_send_secrets = from.fork_send_secrets,
            "fork_require_approval_private" => {
                p.fork_require_approval_private = from.fork_require_approval_private
            }
            "default_token_permission" => {
                p.default_token_permission = from.default_token_permission
            }
            "token_can_approve_change_requests" => {
                p.token_can_approve_change_requests = from.token_can_approve_change_requests
            }
            "artifact_retention_days" => p.artifact_retention_days = from.artifact_retention_days,
            "log_retention_days" => p.log_retention_days = from.log_retention_days,
            "cache_max_size_bytes" => p.cache_max_size_bytes = from.cache_max_size_bytes,
            "cache_retention_days" => p.cache_retention_days = from.cache_retention_days,
            "allowed_patterns" => p.allowed_patterns = from.allowed_patterns.clone(),
            _ => {}
        }
    }
}

/// Fields where `repo` would loosen `org`, by name.
fn loosened(org: &ActionsPolicy, repo: &ActionsPolicy, org_set: &[String]) -> Vec<&'static str> {
    let mut v = Vec::new();
    let has = |n: &str| org_set.iter().any(|s| s == n);
    if has("allowed_actions") && strictness(repo.allowed_actions) < strictness(org.allowed_actions)
    {
        v.push("allowed_actions");
    }
    if has("sha_pinning_required") && org.sha_pinning_required && !repo.sha_pinning_required {
        v.push("sha_pinning_required");
    }
    if has("default_token_permission")
        && org.default_token_permission == ActionsPolicyTokenPermission::Read as i32
        && repo.default_token_permission == ActionsPolicyTokenPermission::Write as i32
    {
        v.push("default_token_permission");
    }
    if has("fork_send_secrets") && !org.fork_send_secrets && repo.fork_send_secrets {
        v.push("fork_send_secrets");
    }
    if has("fork_send_write_tokens") && !org.fork_send_write_tokens && repo.fork_send_write_tokens {
        v.push("fork_send_write_tokens");
    }
    if has("agent_runs_require_approval")
        && org.agent_runs_require_approval
        && !repo.agent_runs_require_approval
    {
        v.push("agent_runs_require_approval");
    }
    if has("artifact_retention_days") && repo.artifact_retention_days > org.artifact_retention_days
    {
        v.push("artifact_retention_days");
    }
    if has("log_retention_days") && repo.log_retention_days > org.log_retention_days {
        v.push("log_retention_days");
    }
    v
}

fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern.eq_ignore_ascii_case(text);
    }
    let t = text.to_lowercase();
    let mut pos = 0;
    for (i, p) in parts.iter().enumerate() {
        let p = p.to_lowercase();
        if i == 0 {
            if !t.starts_with(&p) {
                return false;
            }
            pos = p.len();
        } else if i == parts.len() - 1 {
            return t.len() >= pos + p.len() && t[pos..].ends_with(&p);
        } else if let Some(f) = t[pos..].find(&p) {
            pos += f + p.len();
        } else {
            return false;
        }
    }
    true
}

fn collect_uses(jobs: &[PlannedJob], out: &mut Vec<String>) {
    for j in jobs {
        match &j.body {
            JobBody::Instances(legs) => {
                for l in legs {
                    for s in &l.steps {
                        if let PlannedStepKind::Uses { action, .. } = &s.kind {
                            out.push(action.clone());
                        }
                    }
                }
            }
            JobBody::Call(calls) => {
                for c in calls {
                    if let CalledWorkflow::Planned { jobs, .. } = &c.workflow {
                        collect_uses(jobs, out);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Ci {
    async fn policy_record(&self, cx: &Cx, key: &str) -> Result<Option<PolicyRecord>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get_msg::<PolicyRecord>(&POLICIES, "", key)
            .await?
            .map(|(p, _)| p))
    }

    /// The policy of an org or repo with inherited values (and, for repos, `org_enforced`).
    pub async fn get_policy(&self, cx: &Cx, scope: &PolicyScope) -> Result<ActionsPolicy> {
        let mut p = default_policy(&self.config);
        let org_id = match scope {
            PolicyScope::Org(o) => o.clone(),
            PolicyScope::Repo(r) => r.org_id.clone(),
        };
        let org = self.policy_record(cx, &format!("org:{org_id}")).await?;
        if let Some(o) = &org {
            overlay(&mut p, &o.policy.clone().unwrap_or_default(), &o.set_fields);
        }
        if let PolicyScope::Repo(repo) = scope {
            let org_p = p.clone();
            if let Some(r) = self.policy_record(cx, &scope.key()).await? {
                overlay(&mut p, &r.policy.clone().unwrap_or_default(), &r.set_fields);
            }
            let org_set = org
                .as_ref()
                .map(|o| o.set_fields.clone())
                .unwrap_or_default();
            // Enforced fields: the org value wins where the repo would loosen it.
            let enforced = loosened(&org_p, &p, &org_set);
            for f in &enforced {
                overlay(&mut p, &org_p, &[f.to_string()]);
            }
            p.org_enforced = org_set
                .iter()
                .filter(|f| {
                    matches!(
                        f.as_str(),
                        "allowed_actions"
                            | "sha_pinning_required"
                            | "default_token_permission"
                            | "fork_send_secrets"
                            | "fork_send_write_tokens"
                            | "agent_runs_require_approval"
                            | "enabled_repos"
                    )
                })
                .cloned()
                .collect();
            let repos_mode = ActionsPolicyEnabledRepos::try_from(org_p.enabled_repos)
                .unwrap_or(ActionsPolicyEnabledRepos::All);
            let selected = org
                .as_ref()
                .is_some_and(|o| o.enabled_repo_ids.contains(&repo.id));
            match repos_mode {
                ActionsPolicyEnabledRepos::None => p.enabled = false,
                ActionsPolicyEnabledRepos::Selected if !selected => p.enabled = false,
                _ => {}
            }
        }
        p.scope = Some(scope.proto());
        Ok(p)
    }

    /// The repo's effective policy.
    pub async fn effective_policy(&self, cx: &Cx, repo: &Repo) -> Result<ActionsPolicy> {
        self.get_policy(cx, &PolicyScope::Repo(Box::new(repo.clone())))
            .await
    }

    /// Updates a policy; unset fields are unchanged.
    pub async fn set_policy(
        &self,
        cx: &Cx,
        scope: &PolicyScope,
        req: &SetActionsPolicyRequest,
    ) -> Result<ActionsPolicy> {
        let key = scope.key();
        let (org_id, repo_id) = match scope {
            PolicyScope::Org(o) => (o.clone(), String::new()),
            PolicyScope::Repo(r) => (r.org_id.clone(), r.id.clone()),
        };
        if let PolicyScope::Repo(_) = scope {
            if req.enabled_repos.is_some() {
                return Err(ForgeError::invalid(
                    "invalid_setting",
                    "enabled_repos is an org setting",
                ));
            }
            let org_eff = self
                .get_policy(cx, &PolicyScope::Org(org_id.clone()))
                .await?;
            let org_rec = self.policy_record(cx, &format!("org:{org_id}")).await?;
            let mut would = org_eff.clone();
            if let Some(r) = self.policy_record(cx, &key).await? {
                overlay(
                    &mut would,
                    &r.policy.clone().unwrap_or_default(),
                    &r.set_fields,
                );
            }
            apply_set(&mut would, req);
            let bad = loosened(
                &org_eff,
                &would,
                &org_rec.map(|o| o.set_fields).unwrap_or_default(),
            );
            if !bad.is_empty() {
                return Err(ForgeError::policy_denied(
                    "org_actions_policy",
                    bad.iter().map(|b| b.to_string()).collect(),
                    format!(
                        "the org enforces {}; a repo may not loosen it",
                        bad.join(", ")
                    ),
                ));
            }
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, version) = match tx.get_msg::<PolicyRecord>(&POLICIES, "", &key).await? {
            Some((r, row)) => (r, row.version),
            None => (PolicyRecord::default(), 0),
        };
        let mut p = rec.policy.clone().unwrap_or_default();
        let set = apply_set(&mut p, req);
        for s in set {
            if !rec.set_fields.iter().any(|x| x == s) {
                rec.set_fields.push(s.to_string());
            }
        }
        rec.set_fields.sort();
        rec.policy = Some(p);
        let row = Row::new("", key.clone()).version(version).msg(&rec);
        if version == 0 {
            tx.insert(&POLICIES, &row).await?;
        } else {
            tx.update(&POLICIES, &row).await?;
        }
        tx.audit(
            AuditDraft::new("actions_policy.update", &org_id)
                .repo_id(&repo_id)
                .target_id(&key)
                .after(&json!({ "set": rec.set_fields }).to_string()),
        )
        .await?;
        tx.commit().await?;
        self.get_policy(cx, scope).await
    }

    /// Repos enabled when the org policy uses SELECTED.
    pub async fn enabled_repos(&self, cx: &Cx, org_id: &str) -> Result<Vec<String>> {
        Ok(self
            .policy_record(cx, &format!("org:{org_id}"))
            .await?
            .map(|r| r.enabled_repo_ids)
            .unwrap_or_default())
    }

    /// Replaces the SELECTED repo set.
    pub async fn set_enabled_repos(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_ids: Vec<String>,
    ) -> Result<()> {
        let eff = self
            .get_policy(cx, &PolicyScope::Org(org_id.to_string()))
            .await?;
        if eff.enabled_repos != ActionsPolicyEnabledRepos::Selected as i32 {
            return Err(ForgeError::invalid_state(
                "the org policy does not use SELECTED repos",
            ));
        }
        let key = format!("org:{org_id}");
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, version) = match tx.get_msg::<PolicyRecord>(&POLICIES, "", &key).await? {
            Some((r, row)) => (r, row.version),
            None => (PolicyRecord::default(), 0),
        };
        let mut ids = repo_ids;
        ids.sort();
        ids.dedup();
        rec.enabled_repo_ids = ids;
        let row = Row::new("", key.clone()).version(version).msg(&rec);
        if version == 0 {
            tx.insert(&POLICIES, &row).await?;
        } else {
            tx.update(&POLICIES, &row).await?;
        }
        tx.audit(
            AuditDraft::new("actions_policy.enabled_repos", org_id)
                .target_id(&key)
                .after(&json!({ "repos": rec.enabled_repo_ids }).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// Checks a plan's `uses:` against the policy; `Err` carries the message of the
    /// startup failure.
    pub(crate) fn check_actions_policy(
        &self,
        p: &ActionsPolicy,
        repo: &Repo,
        plan: &PlannedRun,
    ) -> std::result::Result<(), String> {
        let mut uses = Vec::new();
        collect_uses(&plan.jobs, &mut uses);
        self.check_uses(p, repo, &uses)
    }

    /// Checks `uses:` references against the policy; `Err` carries the violation.
    pub fn check_uses(
        &self,
        p: &ActionsPolicy,
        repo: &Repo,
        uses: &[String],
    ) -> std::result::Result<(), String> {
        let owner = repo
            .full_name
            .split('/')
            .next()
            .unwrap_or("")
            .to_lowercase();
        let mode = ActionsPolicyAllowedActions::try_from(p.allowed_actions)
            .unwrap_or(ActionsPolicyAllowedActions::All);
        for u in uses {
            let u = u.clone();
            if u.starts_with("./") {
                continue;
            }
            let local_org = u
                .split('/')
                .next()
                .is_some_and(|o| o.to_lowercase() == owner);
            let allowed = match mode {
                ActionsPolicyAllowedActions::LocalOnly => local_org,
                ActionsPolicyAllowedActions::Selected => {
                    local_org || p.allowed_patterns.iter().any(|pat| glob(pat, &u))
                }
                _ => true,
            };
            if !allowed {
                return Err(format!(
                    "disallowed_action: {u} is not allowed by the actions policy of {}",
                    repo.full_name
                ));
            }
            if p.sha_pinning_required && !u.starts_with("docker://") {
                let r = u.rsplit_once('@').map(|(_, r)| r).unwrap_or("");
                let pinned =
                    (r.len() == 40 || r.len() == 64) && r.bytes().all(|b| b.is_ascii_hexdigit());
                if !pinned {
                    return Err(format!(
                        "disallowed_action: {u} must be pinned to a full commit id (sha_pinning_required)"
                    ));
                }
            }
        }
        Ok(())
    }

    // ----- required workflows -----------------------------------------------------------------

    /// An org's required workflows.
    pub async fn required_workflows(
        &self,
        cx: &Cx,
        org_id: &str,
    ) -> Result<Vec<WorkflowRequirement>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<RequiredRecord>(&REQUIRED, &Scan::scope(org_id, 1000))
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.requirement)
            .collect())
    }

    /// Creates a required workflow after checking the source file exists.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_required(
        &self,
        cx: &Cx,
        org_id: &str,
        name: &str,
        source: &Repo,
        path: &str,
        source_ref: &str,
        scope: WorkflowRequirementScope,
        selected: Vec<Repo>,
        enforced: bool,
    ) -> Result<WorkflowRequirement> {
        if !super::engine::is_ci_workflow_path(path) {
            return Err(ForgeError::invalid(
                "invalid_path",
                format!("{path} is not a workflow file"),
            ));
        }
        self.required_source(source, path, source_ref)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("{path} in {}", source.full_name)))?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let req = WorkflowRequirement {
            id: tx.new_id()?,
            org: org_id.to_string(),
            name: name.to_string(),
            source_repo: repo_ref_of(&source.id),
            source_path: path.to_string(),
            source_ref: source_ref.to_string(),
            scope: scope as i32,
            selected_repos: selected
                .iter()
                .map(|r| repo_ref_of(&r.id).unwrap_or_default())
                .collect(),
            enforced,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        let rec = RequiredRecord {
            requirement: Some(req.clone()),
            source_repo_id: source.id.clone(),
            repo_ids: selected.iter().map(|r| r.id.clone()).collect(),
        };
        tx.insert(&REQUIRED, &Row::new(org_id, req.id.clone()).msg(&rec))
            .await?;
        tx.audit(
            AuditDraft::new("workflow.required_create", org_id)
                .target_id(&req.id)
                .after(
                    &json!({ "name": name, "path": path, "source": source.full_name }).to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(req)
    }

    /// Updates a required workflow.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_required(
        &self,
        cx: &Cx,
        org_id: &str,
        id: &str,
        name: Option<String>,
        path: Option<String>,
        source_ref: Option<String>,
        scope: Option<WorkflowRequirementScope>,
        selected: Option<Vec<Repo>>,
        enforced: Option<bool>,
    ) -> Result<WorkflowRequirement> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut rec, row)) = tx.get_msg::<RequiredRecord>(&REQUIRED, org_id, id).await?
        else {
            return Err(ForgeError::not_found(format!("required workflow {id}")));
        };
        let mut req = rec.requirement.clone().unwrap_or_default();
        if let Some(n) = name {
            req.name = n;
        }
        if let Some(p) = path {
            if !super::engine::is_ci_workflow_path(&p) {
                return Err(ForgeError::invalid(
                    "invalid_path",
                    format!("{p} is not a workflow file"),
                ));
            }
            req.source_path = p;
        }
        if let Some(r) = source_ref {
            req.source_ref = r;
        }
        if let Some(s) = scope {
            req.scope = s as i32;
        }
        if let Some(sel) = selected {
            req.selected_repos = sel
                .iter()
                .map(|r| repo_ref_of(&r.id).unwrap_or_default())
                .collect();
            rec.repo_ids = sel.iter().map(|r| r.id.clone()).collect();
        }
        if let Some(e) = enforced {
            req.enforced = e;
        }
        req.updated_at = Some(ts(tx.now_ms()));
        rec.requirement = Some(req.clone());
        tx.update(
            &REQUIRED,
            &Row::new(org_id, id).version(row.version).msg(&rec),
        )
        .await?;
        tx.audit(AuditDraft::new("workflow.required_update", org_id).target_id(id))
            .await?;
        tx.commit().await?;
        Ok(req)
    }

    /// Removes a required workflow.
    pub async fn delete_required(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&REQUIRED, org_id, id).await? {
            return Err(ForgeError::not_found(format!("required workflow {id}")));
        }
        tx.audit(AuditDraft::new("workflow.required_delete", org_id).target_id(id))
            .await?;
        tx.commit().await
    }

    async fn required_source(&self, source: &Repo, path: &str, r: &str) -> Result<Option<String>> {
        let name = if r.is_empty() {
            super::convert::bee_ref(if source.default_ref.is_empty() {
                "main"
            } else {
                &source.default_ref
            })
        } else {
            super::convert::bee_ref(r)
        };
        let Some(tip) = self.vcs.ref_tip(source, &name).await? else {
            return Ok(None);
        };
        Ok(self
            .vcs
            .read_file(source, &tip, path)
            .await?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    /// Required workflows that apply to `repo`: (requirement id, source path, YAML).
    pub(crate) async fn required_sources(
        &self,
        cx: &Cx,
        repo: &Repo,
    ) -> Result<Vec<(String, String, String)>> {
        let recs = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<RequiredRecord>(&REQUIRED, &Scan::scope(repo.org_id.clone(), 1000))
                .await?
        };
        let mut out = Vec::new();
        for (r, _) in recs {
            let req = r.requirement.clone().unwrap_or_default();
            let applies = req.scope != WorkflowRequirementScope::Selected as i32
                || r.repo_ids.contains(&repo.id);
            if !applies || r.source_repo_id == repo.id {
                continue;
            }
            let Ok(src_repo) = self.repo_by_id(cx, &r.source_repo_id).await else {
                continue;
            };
            if let Some(src) = self
                .required_source(&src_repo, &req.source_path, &req.source_ref)
                .await?
            {
                out.push((req.id.clone(), req.source_path.clone(), src));
            }
        }
        Ok(out)
    }

    /// The enforced required workflows that apply to landing `heads` (hex changeset ids,
    /// the change request's head first) in `repo`, each with whether its latest run on one of
    /// the heads succeeded (F49-007: `enforced` blocks the land until the workflow passes;
    /// evaluate-only requirements are left out).
    pub async fn required_workflow_gates(
        &self,
        cx: &Cx,
        repo: &Repo,
        heads: &[String],
    ) -> Result<Vec<bee_policy::WorkflowGate>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let recs = tx
            .scan_msgs::<RequiredRecord>(&REQUIRED, &Scan::scope(repo.org_id.clone(), 1000))
            .await?;
        let mut enforced = Vec::new();
        for (r, _) in recs {
            let req = r.requirement.clone().unwrap_or_default();
            let applies = req.scope != WorkflowRequirementScope::Selected as i32
                || r.repo_ids.contains(&repo.id);
            if req.enforced && applies && r.source_repo_id != repo.id {
                enforced.push(req);
            }
        }
        if enforced.is_empty() {
            return Ok(Vec::new());
        }
        let mut runs: Vec<(i64, RunRecord)> = Vec::new();
        for h in heads {
            for (rec, row) in tx
                .scan_msgs::<RunRecord>(
                    &RUNS,
                    &Scan::scope(repo.id.clone(), 1000).key(Key::K2(h.clone())),
                )
                .await?
            {
                if !rec.required_workflow_id.is_empty() {
                    runs.push((row.n1.unwrap_or(0), rec));
                }
            }
        }
        Ok(enforced
            .into_iter()
            .map(|req| {
                let latest = runs
                    .iter()
                    .filter(|(_, r)| r.required_workflow_id == req.id)
                    .max_by_key(|(n, _)| *n)
                    .and_then(|(_, r)| r.run.as_ref());
                let passed = latest.is_some_and(|run| {
                    run.status == bee_proto::v1::WorkflowStatus::Completed as i32
                        && run.conclusion == bee_proto::v1::WorkflowConclusion::Success as i32
                });
                bee_policy::WorkflowGate {
                    name: req.name,
                    passed,
                }
            })
            .collect())
    }

    // ----- OIDC -------------------------------------------------------------------------------

    /// The OIDC subject template of an org or repo.
    pub async fn oidc_template(
        &self,
        cx: &Cx,
        scope: &PolicyScope,
    ) -> Result<WorkflowOidcClaimTemplate> {
        let rec = self.policy_record(cx, &scope.key()).await?;
        Ok(match rec.and_then(|r| r.oidc) {
            Some(mut t) => {
                t.scope = Some(scope.proto());
                t
            }
            None => WorkflowOidcClaimTemplate {
                scope: Some(scope.proto()),
                use_default: true,
                include_claim_keys: Vec::new(),
            },
        })
    }

    /// Sets the OIDC subject template.
    pub async fn set_oidc_template(
        &self,
        cx: &Cx,
        scope: &PolicyScope,
        use_default: bool,
        keys: Vec<String>,
    ) -> Result<WorkflowOidcClaimTemplate> {
        for k in &keys {
            if !OIDC_CLAIMS.contains(&k.as_str()) {
                return Err(ForgeError::invalid(
                    "unknown_claim",
                    format!("unknown claim key {k}"),
                ));
            }
        }
        let key = scope.key();
        let org_id = match scope {
            PolicyScope::Org(o) => o.clone(),
            PolicyScope::Repo(r) => r.org_id.clone(),
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, version) = match tx.get_msg::<PolicyRecord>(&POLICIES, "", &key).await? {
            Some((r, row)) => (r, row.version),
            None => (PolicyRecord::default(), 0),
        };
        rec.oidc = Some(WorkflowOidcClaimTemplate {
            scope: Some(scope.proto()),
            use_default,
            include_claim_keys: keys,
        });
        let row = Row::new("", key.clone()).version(version).msg(&rec);
        if version == 0 {
            tx.insert(&POLICIES, &row).await?;
        } else {
            tx.update(&POLICIES, &row).await?;
        }
        tx.audit(AuditDraft::new("actions_policy.oidc_template", &org_id).target_id(&key))
            .await?;
        tx.commit().await?;
        self.oidc_template(cx, scope).await
    }

    /// The GitHub-compatible claims of a job's OIDC token (without `aud`, times and `jti`).
    pub async fn oidc_claims(
        &self,
        cx: &Cx,
        repo: &Repo,
        job: &JobRecord,
    ) -> Result<BTreeMap<String, String>> {
        let w = job_of(job);
        let rec = self.run_record(cx, &repo.id, &w.run_id).await?;
        let run = run_of(&rec);
        let event = rec.event.clone().unwrap_or_default();
        let git = event.git_ref.clone();
        let ref_type = if git.starts_with("refs/tags/") {
            "tag"
        } else {
            "branch"
        };
        let wf_ref = format!("{}/{}@{}", repo.full_name, run.workflow_path, git);
        let owner = repo.full_name.split('/').next().unwrap_or("").to_string();
        let mut c = BTreeMap::new();
        c.insert(
            "actor".into(),
            run.triggering_actor
                .as_ref()
                .map(|a| a.id.clone())
                .unwrap_or_default(),
        );
        c.insert(
            "actor_id".into(),
            run.triggering_actor
                .as_ref()
                .map(|a| a.id.clone())
                .unwrap_or_default(),
        );
        c.insert("base_ref".into(), event.base_ref.clone());
        c.insert("environment".into(), job.environment.clone());
        c.insert("event_name".into(), event.name.clone());
        c.insert("head_ref".into(), event.head_ref.clone());
        c.insert("job_workflow_ref".into(), wf_ref.clone());
        c.insert("job_workflow_sha".into(), event.sha.clone());
        c.insert("ref".into(), git.clone());
        c.insert("ref_type".into(), ref_type.into());
        c.insert("repository".into(), repo.full_name.clone());
        c.insert("repository_id".into(), repo.id.clone());
        c.insert("repository_owner".into(), owner.clone());
        c.insert("repository_owner_id".into(), repo.org_id.clone());
        c.insert(
            "repository_visibility".into(),
            match repo.visibility {
                v if v == bee_proto::v1::Visibility::Public as i32 => "public".into(),
                v if v == bee_proto::v1::Visibility::Internal as i32 => "internal".into(),
                _ => "private".to_string(),
            },
        );
        c.insert("run_attempt".into(), run.run_attempt.to_string());
        c.insert("run_id".into(), rec.numeric_id.to_string());
        c.insert("run_number".into(), run.run_number.to_string());
        c.insert("runner_environment".into(), "self-hosted".into());
        c.insert("sha".into(), event.sha.clone());
        c.insert("workflow".into(), run.name.clone());
        c.insert("workflow_ref".into(), wf_ref);
        c.insert("workflow_sha".into(), event.sha.clone());
        let context = if !job.environment.is_empty() {
            format!("environment:{}", job.environment)
        } else if event.name == "pull_request" {
            "pull_request".to_string()
        } else {
            format!("ref:{git}")
        };
        c.insert("context".into(), context.clone());
        c.insert("repo".into(), repo.full_name.clone());
        let template = self
            .oidc_template(cx, &PolicyScope::Repo(Box::new(repo.clone())))
            .await?;
        let template = if template.use_default {
            self.oidc_template(cx, &PolicyScope::Org(repo.org_id.clone()))
                .await?
        } else {
            template
        };
        let sub = if template.use_default || template.include_claim_keys.is_empty() {
            format!("repo:{}:{context}", repo.full_name)
        } else {
            template
                .include_claim_keys
                .iter()
                .map(|k| format!("{k}:{}", c.get(k).cloned().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(":")
        };
        c.remove("context");
        c.remove("repo");
        c.insert("sub".into(), sub);
        c.insert("iss".into(), self.config.oidc_issuer.clone());
        Ok(c)
    }

    /// Issues a signed OIDC JWT for a running job; `PERMISSION_DENIED` unless the job has
    /// `id-token: write`.
    pub async fn oidc_token(
        &self,
        cx: &Cx,
        repo: &Repo,
        job_id: &str,
        audience: &str,
    ) -> Result<(String, u64)> {
        let job = self.job_record(cx, &repo.id, job_id).await?;
        if super::engine::status_of(job_of(&job)) != bee_proto::v1::WorkflowStatus::InProgress {
            return Err(ForgeError::invalid_state("the job is not running"));
        }
        if !super::convert::has_permission(
            &job.permissions,
            "id-token",
            bee_workflow::model::Access::Write,
        ) {
            return Err(ForgeError::permission_denied(
                "the job's permissions do not include `id-token: write`",
            ));
        }
        let claims = self.oidc_claims(cx, repo, &job).await?;
        let now = self.now();
        let exp = now + self.config.oidc_ttl_ms;
        let aud = if audience.is_empty() {
            format!(
                "{}/{}",
                self.config.server_url,
                repo.full_name.split('/').next().unwrap_or("")
            )
        } else {
            audience.to_string()
        };
        let mut payload = serde_json::Map::new();
        for (k, v) in &claims {
            payload.insert(k.clone(), json!(v));
        }
        payload.insert("aud".into(), json!(aud));
        payload.insert("iat".into(), json!(now / 1000));
        payload.insert("nbf".into(), json!(now / 1000));
        payload.insert("exp".into(), json!(exp / 1000));
        payload.insert("jti".into(), json!(self.db.ids().ulid()?));
        Ok((self.jws(&serde_json::Value::Object(payload))?, exp))
    }

    /// A compact JWS of `payload` signed with the key port.
    pub fn jws(&self, payload: &serde_json::Value) -> Result<String> {
        let header =
            json!({ "alg": self.keys.algorithm(), "typ": "JWT", "kid": self.keys.key_id() });
        let signing = format!(
            "{}.{}",
            b64url(header.to_string().as_bytes()),
            b64url(payload.to_string().as_bytes())
        );
        let sig = self.keys.sign(signing.as_bytes())?;
        Ok(format!("{signing}.{}", b64url(&sig)))
    }

    /// Verifies a compact JWS made by [`Ci::jws`]; returns its payload.
    pub fn verify_jws(&self, token: &str) -> Option<serde_json::Value> {
        let (signing, sig) = token.rsplit_once('.')?;
        let sig = super::util::b64_decode(sig)?;
        if !self.keys.verify(signing.as_bytes(), &sig) {
            return None;
        }
        let (_, payload) = signing.split_once('.')?;
        serde_json::from_slice(&super::util::b64_decode(payload)?).ok()
    }

    // ----- attestations -----------------------------------------------------------------------

    /// Signs and stores an attestation for a running job with `attestations: write`.
    pub async fn create_attestation(
        &self,
        cx: &Cx,
        repo: &Repo,
        job_id: &str,
        subjects: Vec<AttestationSubject>,
        predicate_type: &str,
        predicate_json: &str,
    ) -> Result<Attestation> {
        if subjects.is_empty() {
            return Err(ForgeError::missing("subjects"));
        }
        for s in &subjects {
            if !s.digest.starts_with("sha256:") || s.digest.len() != 71 {
                return Err(ForgeError::invalid(
                    "bad_digest",
                    format!("{} is not sha256:<hex>", s.digest),
                ));
            }
        }
        let job = self.job_record(cx, &repo.id, job_id).await?;
        if super::engine::status_of(job_of(&job)) != bee_proto::v1::WorkflowStatus::InProgress {
            return Err(ForgeError::invalid_state("the job is not running"));
        }
        if !super::convert::has_permission(
            &job.permissions,
            "attestations",
            bee_workflow::model::Access::Write,
        ) {
            return Err(ForgeError::permission_denied(
                "the job's permissions do not include `attestations: write`",
            ));
        }
        let rec = self.run_record(cx, &repo.id, &job_of(&job).run_id).await?;
        let run = run_of(&rec);
        let event = rec.event.clone().unwrap_or_default();
        let predicate_type = if predicate_type.is_empty() {
            "https://slsa.dev/provenance/v1"
        } else {
            predicate_type
        };
        let signer = format!("{}/{}@{}", repo.full_name, run.workflow_path, event.git_ref);
        let predicate: serde_json::Value = if predicate_json.is_empty() {
            json!({
                "buildDefinition": {
                    "buildType": "https://beehive.dev/actions/workflow/v1",
                    "externalParameters": { "workflow": { "ref": event.git_ref, "repository": repo.full_name, "path": run.workflow_path } },
                    "resolvedDependencies": [{ "uri": format!("bee+{}/{}", self.config.server_url, repo.full_name), "digest": { "beeCommit": event.sha } }],
                },
                "runDetails": {
                    "builder": { "id": signer },
                    "metadata": { "invocationId": format!("{}/attempts/{}", run.html_url, run.run_attempt) },
                },
            })
        } else {
            serde_json::from_str(predicate_json)
                .map_err(|e| ForgeError::invalid("bad_json", format!("predicate_json: {e}")))?
        };
        let statement = json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": subjects.iter().map(|s| json!({
                "name": s.name,
                "digest": { "sha256": s.digest.trim_start_matches("sha256:") },
            })).collect::<Vec<_>>(),
            "predicateType": predicate_type,
            "predicate": predicate,
        });
        let payload = statement.to_string();
        let pae = dsse_pae("application/vnd.in-toto+json", payload.as_bytes());
        let sig = self.keys.sign(&pae)?;
        let bundle = json!({
            "mediaType": "application/vnd.dev.sigstore.bundle+json;version=0.3",
            "verificationMaterial": { "publicKey": { "hint": self.keys.key_id() } },
            "dsseEnvelope": {
                "payload": b64(payload.as_bytes()),
                "payloadType": "application/vnd.in-toto+json",
                "signatures": [{ "keyid": self.keys.key_id(), "sig": b64(&sig) }],
            },
        });
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let att = Attestation {
            id: tx.new_id()?,
            repo: repo_ref_of(&repo.id),
            subjects: subjects.clone(),
            predicate_type: predicate_type.to_string(),
            predicate_json: statement["predicate"].to_string(),
            bundle_json: bundle.to_string(),
            workflow_run_id: run.id.clone(),
            job_id: job_id.to_string(),
            workflow_path: run.workflow_path.clone(),
            source_ref: event.git_ref.clone(),
            source_changeset: super::engine::object_id(&event.sha),
            signer_identity: signer,
            provenance: None,
            created_at: Some(ts(tx.now_ms())),
        };
        tx.insert(
            &ATTESTATIONS,
            &Row::new(repo.id.clone(), att.id.clone())
                .k1(subjects[0].digest.clone())
                .k2(repo.org_id.clone())
                .msg(&AttestationRecord {
                    attestation: Some(att.clone()),
                    org_id: repo.org_id.clone(),
                }),
        )
        .await?;
        tx.audit(
            AuditDraft::new("attestation.create", &repo.org_id)
                .repo_id(&repo.id)
                .target_id(&att.id)
                .after(
                    &json!({ "subjects": subjects.iter().map(|s| &s.digest).collect::<Vec<_>>() })
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(att)
    }

    /// Attestations of a repo (or of an org when `repo_id` is empty) by subject digest and
    /// predicate type.
    pub async fn list_attestations(
        &self,
        cx: &Cx,
        repo_id: &str,
        org_id: &str,
        digest: &str,
        predicate_type: &str,
    ) -> Result<Vec<Attestation>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let scan = if repo_id.is_empty() {
            Scan {
                key: Some(Key::K2(org_id.to_string())),
                limit: 1000,
                ..Scan::default()
            }
        } else {
            Scan::scope(repo_id, 1000)
        };
        Ok(tx
            .scan_msgs::<AttestationRecord>(&ATTESTATIONS, &scan)
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.attestation)
            .filter(|a| digest.is_empty() || a.subjects.iter().any(|s| s.digest == digest))
            .filter(|a| predicate_type.is_empty() || a.predicate_type == predicate_type)
            .collect())
    }

    /// Verifies attestations of a digest: signature, signer workflow and source ref.
    #[allow(clippy::too_many_arguments)]
    pub async fn verify_attestations(
        &self,
        cx: &Cx,
        repo_id: &str,
        org_id: &str,
        digest: &str,
        predicate_type: &str,
        signer_workflow: &str,
        source_ref: &str,
    ) -> Result<(Vec<Attestation>, Vec<String>)> {
        let all = self
            .list_attestations(cx, repo_id, org_id, digest, predicate_type)
            .await?;
        let mut ok = Vec::new();
        let mut failures = Vec::new();
        for a in all {
            match self.check_bundle(&a) {
                Err(e) => failures.push(format!("{}: {e}", a.id)),
                Ok(()) => {
                    let signer_wf = a
                        .signer_identity
                        .rsplit_once('@')
                        .map(|(w, _)| w)
                        .unwrap_or("");
                    if !signer_workflow.is_empty() && signer_wf != signer_workflow {
                        failures.push(format!(
                            "{}: signed by {signer_wf}, expected {signer_workflow}",
                            a.id
                        ));
                    } else if !source_ref.is_empty() && a.source_ref != source_ref {
                        failures.push(format!(
                            "{}: built from {}, expected {source_ref}",
                            a.id, a.source_ref
                        ));
                    } else {
                        ok.push(a);
                    }
                }
            }
        }
        if ok.is_empty() && failures.is_empty() {
            failures.push(format!("no attestations for {digest}"));
        }
        Ok((ok, failures))
    }

    fn check_bundle(&self, a: &Attestation) -> std::result::Result<(), String> {
        let b: serde_json::Value =
            serde_json::from_str(&a.bundle_json).map_err(|e| e.to_string())?;
        let env = &b["dsseEnvelope"];
        let payload = env["payload"]
            .as_str()
            .and_then(super::util::b64_decode)
            .ok_or("bundle has no payload")?;
        let sig = env["signatures"][0]["sig"]
            .as_str()
            .and_then(super::util::b64_decode)
            .ok_or("bundle has no signature")?;
        let pae = dsse_pae(env["payloadType"].as_str().unwrap_or(""), &payload);
        if !self.keys.verify(&pae, &sig) {
            return Err("signature does not verify".into());
        }
        let st: serde_json::Value = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
        let digests: Vec<String> = st["subject"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(|x| {
                        x["digest"]["sha256"]
                            .as_str()
                            .map(|d| format!("sha256:{d}"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !a.subjects.iter().all(|s| digests.contains(&s.digest)) {
            return Err("subjects do not match the signed statement".into());
        }
        Ok(())
    }

    /// Deletes an attestation.
    pub async fn delete_attestation(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx.delete(&ATTESTATIONS, &repo.id, id).await? {
            return Err(ForgeError::not_found(format!("attestation {id}")));
        }
        tx.audit(
            AuditDraft::new("attestation.delete", &repo.org_id)
                .repo_id(&repo.id)
                .target_id(id),
        )
        .await?;
        tx.commit().await
    }

    // ----- usage and badges -------------------------------------------------------------------

    /// Time used by a workflow's runs since the billing period start.
    pub async fn workflow_usage(
        &self,
        cx: &Cx,
        repo_id: &str,
        workflow_id: &str,
    ) -> Result<WorkflowUsage> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<UsageRecord>(
                &USAGE,
                &Scan::scope(repo_id, crate::store::MAX_SCAN).key(Key::K1(workflow_id.to_string())),
            )
            .await?;
        let start = self.config.billing_period_start_ms;
        let mut by: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        let mut runs: BTreeMap<String, u64> = BTreeMap::new();
        for (u, _) in rows.iter().filter(|(u, _)| u.completed_ms >= start) {
            let e = by.entry(u.machine_class.clone()).or_default();
            e.0 += u.billable_ms;
            e.1 += 1;
            *runs.entry(u.run_id.clone()).or_default() += u.duration_ms;
        }
        Ok(WorkflowUsage {
            workflow_id: workflow_id.to_string(),
            runners: by
                .into_iter()
                .map(|(k, (b, n))| WorkflowRunnerUsage {
                    machine_class: k,
                    billable_ms: b,
                    job_count: n,
                })
                .collect(),
            run_duration_ms: runs.values().sum(),
            period_start_at: Some(ts(start)),
        })
    }

    /// Time used by one run.
    pub async fn run_usage(
        &self,
        cx: &Cx,
        repo_id: &str,
        run_id: &str,
    ) -> Result<WorkflowRunUsage> {
        let rec = self.run_record(cx, repo_id, run_id).await?;
        let run = run_of(&rec);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<UsageRecord>(
                &USAGE,
                &Scan::scope(repo_id, crate::store::MAX_SCAN).key(Key::K1(run.workflow_id.clone())),
            )
            .await?;
        let mine: Vec<UsageRecord> = rows
            .into_iter()
            .map(|(u, _)| u)
            .filter(|u| u.run_id == run_id)
            .collect();
        let mut by: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for u in &mine {
            let e = by.entry(u.machine_class.clone()).or_default();
            e.0 += u.billable_ms;
            e.1 += 1;
        }
        let duration = match (run.run_started_at.as_ref(), run.completed_at.as_ref()) {
            (Some(s), Some(c)) => crate::ids::ts_ms(c).saturating_sub(crate::ids::ts_ms(s)),
            _ => mine.iter().map(|u| u.duration_ms).sum(),
        };
        Ok(WorkflowRunUsage {
            run_id: run_id.to_string(),
            run_duration_ms: duration,
            runners: by
                .into_iter()
                .map(|(k, (b, n))| WorkflowRunnerUsage {
                    machine_class: k,
                    billable_ms: b,
                    job_count: n,
                })
                .collect(),
            jobs: mine
                .iter()
                .map(|u| WorkflowJobUsage {
                    job_id: u.job_id.clone(),
                    machine_class: u.machine_class.clone(),
                    duration_ms: u.duration_ms,
                    billable_ms: u.billable_ms,
                })
                .collect(),
        })
    }

    /// Status badge data of a workflow: the latest completed run on `branch` (the default
    /// branch when empty).
    pub async fn badge(
        &self,
        cx: &Cx,
        repo: &Repo,
        workflow_id: &str,
        branch: &str,
    ) -> Result<Badge> {
        let branch = if branch.is_empty() {
            super::convert::bee_ref(if repo.default_ref.is_empty() {
                "main"
            } else {
                &repo.default_ref
            })
        } else {
            branch.to_string()
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let name = tx
            .get_msg::<super::model::WorkflowRow>(&WORKFLOWS, &repo.id, workflow_id)
            .await?
            .and_then(|(w, _)| w.workflow)
            .map(|w| w.name)
            .ok_or_else(|| ForgeError::not_found(format!("workflow {workflow_id}")))?;
        let rows = tx
            .scan_msgs::<RunRecord>(
                &RUNS,
                &Scan::scope(repo.id.clone(), 1000).key(Key::K1(workflow_id.to_string())),
            )
            .await?;
        let latest = rows
            .iter()
            .map(|(r, _)| run_of(r))
            .filter(|r| {
                r.head_ref == branch && r.status == bee_proto::v1::WorkflowStatus::Completed as i32
            })
            .max_by_key(|r| r.id.clone());
        let (message, color) = match latest.map(|r| r.conclusion) {
            Some(c) if c == bee_proto::v1::WorkflowConclusion::Success as i32 => {
                ("passing", "#2ea44f")
            }
            Some(c) if c == bee_proto::v1::WorkflowConclusion::Cancelled as i32 => {
                ("cancelled", "#9f9f9f")
            }
            Some(_) => ("failing", "#cb2431"),
            None => ("no status", "#9f9f9f"),
        };
        Ok(Badge {
            label: name,
            message: message.into(),
            color: color.into(),
        })
    }
}

/// Status badge data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Badge {
    /// Left text (the workflow name).
    pub label: String,
    /// Right text (`passing`, `failing`, `cancelled`, `no status`).
    pub message: String,
    /// Right colour.
    pub color: String,
}

impl Badge {
    /// The badge as SVG.
    pub fn svg(&self) -> String {
        let esc = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let lw = 6 * self.label.chars().count() as u32 + 10;
        let mw = 6 * self.message.chars().count() as u32 + 10;
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"20\" role=\"img\" aria-label=\"{l}: {m}\"><rect width=\"{lw}\" height=\"20\" fill=\"#555\"/><rect x=\"{lw}\" width=\"{mw}\" height=\"20\" fill=\"{c}\"/><g fill=\"#fff\" font-family=\"Verdana,sans-serif\" font-size=\"11\"><text x=\"5\" y=\"14\">{l}</text><text x=\"{mx}\" y=\"14\">{m}</text></g></svg>",
            w = lw + mw,
            l = esc(&self.label),
            m = esc(&self.message),
            c = self.color,
            mx = lw + 5,
        )
    }
}

/// OIDC claim keys a subject template may include.
pub const OIDC_CLAIMS: &[&str] = &[
    "actor",
    "actor_id",
    "base_ref",
    "context",
    "environment",
    "event_name",
    "head_ref",
    "job_workflow_ref",
    "job_workflow_sha",
    "ref",
    "ref_type",
    "repo",
    "repository",
    "repository_id",
    "repository_owner",
    "repository_owner_id",
    "repository_visibility",
    "run_attempt",
    "run_id",
    "run_number",
    "runner_environment",
    "sha",
    "workflow",
    "workflow_ref",
    "workflow_sha",
];

/// DSSE pre-authentication encoding.
fn dsse_pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut v = format!(
        "DSSEv1 {} {} {} ",
        payload_type.len(),
        payload_type,
        payload.len()
    )
    .into_bytes();
    v.extend_from_slice(payload);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_action_patterns() {
        assert!(glob("actions/*", "actions/checkout@v4"));
        assert!(glob("actions/checkout@*", "actions/checkout@v4"));
        assert!(!glob("octo/*", "actions/checkout@v4"));
        assert!(glob("*/setup-*@v1", "actions/setup-node@v1"));
    }

    #[test]
    fn repos_cannot_loosen_org_settings() {
        let org = ActionsPolicy {
            allowed_actions: ActionsPolicyAllowedActions::LocalOnly as i32,
            sha_pinning_required: true,
            ..Default::default()
        };
        let repo = ActionsPolicy {
            allowed_actions: ActionsPolicyAllowedActions::All as i32,
            ..Default::default()
        };
        let set = vec![
            "allowed_actions".to_string(),
            "sha_pinning_required".to_string(),
        ];
        assert_eq!(
            loosened(&org, &repo, &set),
            vec!["allowed_actions", "sha_pinning_required"]
        );
        assert!(loosened(&org, &org, &set).is_empty());
    }
}
