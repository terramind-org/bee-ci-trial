//! The agent registry: runtimes, agent definitions, owner budgets, principal summaries,
//! dashboards, work items and the activity stream.
//!
//! - A **runtime** (`AgentRuntime`) describes how to run an agent kind: name, vendor,
//!   version, image, sandbox needs, default models. Runtimes belong to an org; public ones
//!   (org `""`) are registered by operators through [`Agents::register_runtime`].
//! - An **agent definition** ([`AgentDefinition`]) binds an agent principal (created through
//!   `AuthService.CreateAgent`) to an org: name, runtime, model, instructions file, allowed
//!   toolsets (policy actions the run token may carry), default per-run budget, owners and
//!   schedules. Only agents with an enabled definition take work.
//! - An **owner budget** ([`OwnerBudget`]) caps what one owner's agents spend per UTC month;
//!   runs refuse to start (`BUDGET_EXHAUSTED`) once it is spent.

use std::collections::{BTreeMap, BTreeSet};

use bee_proto::v1::{
    AgentDailyStats, AgentDashboard, AgentOutcomes, AgentPrincipalSummary, AgentRun,
    AgentRunBudget, AgentRunState, AgentRuntime, AgentSandboxRequirements, AgentStats,
    AgentWorkItem, Cost, Event, Principal, PrincipalKind, RepoRef, SubjectKind, SubjectRef,
    repo_ref,
};

use super::model::{
    AgentCrRec, AgentReviewRec, DefRec, OwnerBudgetRec, RunRec, ScheduleRec, SessionRec, SpendRec,
};
use super::*;
use crate::audit::AuditDraft;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts, ts_ms};
use crate::store::{Key, Row, Scan, Tx};

/// Most runtimes per org.
pub const MAX_RUNTIMES: usize = 200;
/// Most agent definitions per org.
pub const MAX_DEFINITIONS: usize = 1000;
/// Most schedules per definition.
pub const MAX_SCHEDULES: usize = 20;

/// A scheduled run of an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    /// Schedule id (assigned when empty).
    pub id: String,
    /// Repo the run works in.
    pub repo_id: String,
    /// Five-field cron expression, UTC.
    pub cron: String,
    /// The task prompt.
    pub prompt: String,
    /// When it last fired (ms; read-only).
    pub last_fired_ms: u64,
}

/// An agent definition: what an agent principal is, runs with, may do and may spend.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentDefinition {
    /// Org.
    pub org_id: String,
    /// The agent principal (kind agent).
    pub agent: Principal,
    /// Name, unique per org.
    pub name: String,
    /// Registered runtime id, if any.
    pub runtime_id: String,
    /// Runtime name: `claude-code`, `codex` or a custom name.
    pub runtime: String,
    /// Default model.
    pub model: String,
    /// Instructions file in the repo (read in addition to AGENTS.md and friends).
    pub instructions_path: String,
    /// Allowed toolsets: policy action names (`read`, `commit`, `lease`, `review`, ...).
    pub toolsets: Vec<String>,
    /// Default per-run budget; a run's budget is never larger.
    pub default_budget: AgentRunBudget,
    /// Owners, primary first. The primary owner's budget pays for runs.
    pub owners: Vec<String>,
    /// What the agent is for.
    pub purpose: String,
    /// Takes work (assignment, mentions, watchers, schedules).
    pub enabled: bool,
    /// Scheduled runs.
    pub schedules: Vec<Schedule>,
    /// Created (ms).
    pub created_ms: u64,
    /// Updated (ms).
    pub updated_ms: u64,
}

impl AgentDefinition {
    /// The primary owner.
    pub fn owner(&self) -> &str {
        self.owners.first().map(String::as_str).unwrap_or("")
    }

    pub(crate) fn from_rec(r: &DefRec) -> AgentDefinition {
        AgentDefinition {
            org_id: r.org_id.clone(),
            agent: r.agent.clone().unwrap_or_default(),
            name: r.name.clone(),
            runtime_id: r.runtime_id.clone(),
            runtime: r.runtime.clone(),
            model: r.model.clone(),
            instructions_path: r.instructions_path.clone(),
            toolsets: r.toolsets.clone(),
            default_budget: r.default_budget.unwrap_or_default(),
            owners: r.owners.clone(),
            purpose: r.purpose.clone(),
            enabled: r.enabled,
            schedules: r
                .schedules
                .iter()
                .map(|s| Schedule {
                    id: s.id.clone(),
                    repo_id: s.repo_id.clone(),
                    cron: s.cron.clone(),
                    prompt: s.prompt.clone(),
                    last_fired_ms: s.last_fired_ms,
                })
                .collect(),
            created_ms: r.created_ms,
            updated_ms: r.updated_ms,
        }
    }
}

/// A partial update of a definition; `None` leaves a field unchanged.
#[derive(Clone, Debug, Default)]
pub struct DefinitionPatch {
    /// New name.
    pub name: Option<String>,
    /// New runtime id (resolves the runtime name).
    pub runtime_id: Option<String>,
    /// New runtime name.
    pub runtime: Option<String>,
    /// New model.
    pub model: Option<String>,
    /// New instructions path.
    pub instructions_path: Option<String>,
    /// New toolsets.
    pub toolsets: Option<Vec<String>>,
    /// New default budget.
    pub default_budget: Option<AgentRunBudget>,
    /// New owners.
    pub owners: Option<Vec<String>>,
    /// New purpose.
    pub purpose: Option<String>,
    /// Enable or disable.
    pub enabled: Option<bool>,
    /// New schedules (replace all).
    pub schedules: Option<Vec<Schedule>>,
}

/// Monthly limits of one owner's agents; 0 = unlimited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnerBudget {
    /// USD micros per UTC month.
    pub max_usd_micros: u64,
    /// Input tokens per month.
    pub max_input_tokens: u64,
    /// Output tokens per month.
    pub max_output_tokens: u64,
    /// Runs started per month.
    pub max_runs: u64,
}

/// What an owner's agents spent this month.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnerSpend {
    /// USD micros.
    pub usd_micros: u64,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Runs started.
    pub runs: u64,
}

impl OwnerBudget {
    /// The limit `spend` has reached, if any.
    pub fn exhausted_by(&self, spend: &OwnerSpend) -> Option<&'static str> {
        let hit = |max: u64, used: u64| max > 0 && used >= max;
        if hit(self.max_usd_micros, spend.usd_micros) {
            Some("usd")
        } else if hit(self.max_input_tokens, spend.input_tokens) {
            Some("input_tokens")
        } else if hit(self.max_output_tokens, spend.output_tokens) {
            Some("output_tokens")
        } else if hit(self.max_runs, spend.runs) {
            Some("runs")
        } else {
            None
        }
    }
}

/// Which events the activity stream returns.
#[derive(Clone, Debug, Default)]
pub struct ActivityFilter {
    /// Org whose log is read.
    pub org_id: String,
    /// Only events acted by this agent.
    pub agent_id: String,
    /// Only events whose delegation chain ends at this principal.
    pub delegation_root: String,
    /// Kind filters (exact, `prefix.*`, `*`); empty = all.
    pub kinds: Vec<String>,
    /// First sequence to read (0 = oldest retained).
    pub from_seq: u64,
    /// Most events returned.
    pub limit: usize,
}

pub(crate) fn runtime_key(name: &str, version: &str) -> String {
    format!("{}@{}", lc(name), version.trim())
}

pub(crate) fn check_runtime_name(name: &str) -> Result<String> {
    let n = lc(name);
    if n.is_empty() {
        return Err(ForgeError::missing("name"));
    }
    if n.len() > 64
        || !n
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
    {
        return Err(ForgeError::invalid(
            "invalid_name",
            "runtime names use lower-case letters, digits, '-', '_' and '.' (at most 64)",
        ));
    }
    Ok(n)
}

pub(crate) fn check_toolsets(t: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for s in t {
        let a = bee_policy::Action::parse(s.trim()).ok_or_else(|| {
            ForgeError::invalid("invalid_toolset", format!("unknown toolset {s:?}"))
        })?;
        if matches!(
            a,
            bee_policy::Action::Purge
                | bee_policy::Action::Redact
                | bee_policy::Action::ManagePolicy
        ) {
            return Err(ForgeError::invalid(
                "invalid_toolset",
                format!("agents cannot be granted `{a}`"),
            ));
        }
        if !out.iter().any(|x| x == a.as_str()) {
            out.push(a.as_str().to_string());
        }
    }
    Ok(out)
}

fn check_path(field: &str, p: &str) -> Result<()> {
    if p.starts_with('/') || p.split('/').any(|c| c == "..") || p.len() > 1024 {
        return Err(ForgeError::invalid(
            "invalid_path",
            format!("{field} must be a relative repo path"),
        ));
    }
    Ok(())
}

pub(crate) fn is_active(s: i32) -> bool {
    matches!(
        AgentRunState::try_from(s).unwrap_or(AgentRunState::Unspecified),
        AgentRunState::Queued
            | AgentRunState::Provisioning
            | AgentRunState::Running
            | AgentRunState::WaitingForReview
    )
}

pub(crate) fn add_cost(a: &mut Cost, b: &Cost) {
    a.input_tokens = a.input_tokens.saturating_add(b.input_tokens);
    a.output_tokens = a.output_tokens.saturating_add(b.output_tokens);
    a.usd_micros = a.usd_micros.saturating_add(b.usd_micros);
}

/// Adds `cost` (and `runs` started) to an owner's month.
pub(crate) async fn charge(
    tx: &mut Tx,
    org_id: &str,
    owner: &str,
    cost: &Cost,
    runs: u64,
) -> Result<()> {
    if owner.is_empty() {
        return Ok(());
    }
    let id = format!("{owner}/{}", utc_month(tx.now_ms()));
    let (mut s, version) = match tx.get_msg::<SpendRec>(&SPEND, org_id, &id).await? {
        Some((s, row)) => (s, row.version),
        None => (SpendRec::default(), 0),
    };
    s.usd_micros = s.usd_micros.saturating_add(cost.usd_micros);
    s.input_tokens = s.input_tokens.saturating_add(cost.input_tokens);
    s.output_tokens = s.output_tokens.saturating_add(cost.output_tokens);
    s.runs = s.runs.saturating_add(runs);
    let row = Row::new(org_id, id).msg(&s);
    if version == 0 {
        tx.insert(&SPEND, &row).await
    } else {
        tx.update(&SPEND, &row.version(version)).await.map(|_| ())
    }
}

/// Fails with `BUDGET_EXHAUSTED` when the owner's monthly budget is spent.
pub(crate) async fn ensure_budget(tx: &mut Tx, org_id: &str, owner: &str) -> Result<()> {
    if owner.is_empty() {
        return Ok(());
    }
    let Some((b, _)) = tx
        .get_msg::<OwnerBudgetRec>(&OWNER_BUDGETS, org_id, owner)
        .await?
    else {
        return Ok(());
    };
    let spend = tx
        .get_msg::<SpendRec>(
            &SPEND,
            org_id,
            &format!("{owner}/{}", utc_month(tx.now_ms())),
        )
        .await?
        .map(|(s, _)| s)
        .unwrap_or_default();
    let budget = OwnerBudget {
        max_usd_micros: b.max_usd_micros,
        max_input_tokens: b.max_input_tokens,
        max_output_tokens: b.max_output_tokens,
        max_runs: b.max_runs,
    };
    let spent = OwnerSpend {
        usd_micros: spend.usd_micros,
        input_tokens: spend.input_tokens,
        output_tokens: spend.output_tokens,
        runs: spend.runs,
    };
    if let Some(which) = budget.exhausted_by(&spent) {
        return Err(ForgeError::new(
            bee_proto::v1::ErrorReason::BudgetExhausted,
            format!("the agent budget of {owner} is spent for this month ({which})"),
        )
        .with_subcode("owner_budget_exhausted"));
    }
    Ok(())
}

pub(crate) async fn locate(tx: &mut Tx, kind: &str, id: &str) -> Result<Option<String>> {
    Ok(tx.get(&LOCATOR, kind, id).await?.and_then(|r| r.k1))
}

pub(crate) async fn put_locator(tx: &mut Tx, kind: &str, id: &str, scope: &str) -> Result<()> {
    tx.put(&LOCATOR, &Row::new(kind, id).k1(scope))
        .await
        .map(|_| ())
}

pub(crate) async fn insert_locator(tx: &mut Tx, kind: &str, id: &str, scope: &str) -> Result<()> {
    tx.insert(&LOCATOR, &Row::new(kind, id).k1(scope))
        .await
        .map(|_| ())
}

pub(crate) fn repo_ref_id(id: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(id.to_string())),
    })
}

impl Agents {
    // ---- runtimes ----

    /// Registers a runtime for `org_id` (`""` = a public marketplace runtime).
    pub async fn register_runtime(
        &self,
        cx: &Cx,
        org_id: &str,
        mut rt: AgentRuntime,
    ) -> Result<AgentRuntime> {
        rt.name = check_runtime_name(&rt.name)?;
        check_len("description", &rt.description, 4096)?;
        check_len("version", &rt.version, 64)?;
        rt.version = rt.version.trim().to_string();
        let key = runtime_key(&rt.name, &rt.version);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx
            .scan(&RUNTIMES, &Scan::scope(org_id, 1).key(Key::K1(key.clone())))
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("runtime {key}")));
        }
        if tx.count(&RUNTIMES, org_id, None).await? as usize >= MAX_RUNTIMES {
            return Err(ForgeError::too_large(
                "runtimes",
                MAX_RUNTIMES as u64,
                MAX_RUNTIMES as u64 + 1,
            ));
        }
        let now = tx.now_ms();
        rt.id = tx.new_id()?;
        rt.org = org_id.to_string();
        rt.enabled = true;
        rt.registered_by = Some(cx.principal());
        rt.created_at = Some(ts(now));
        rt.updated_at = Some(ts(now));
        tx.insert(
            &RUNTIMES,
            &Row::new(org_id, rt.id.clone())
                .k1(key)
                .n1(now as i64)
                .msg(&rt),
        )
        .await?;
        put_locator(&mut tx, "runtime", &rt.id, org_id).await?;
        self.record_org(
            &mut tx,
            org_id,
            "agent_runtime.registered",
            "agent.runtime_register",
            &rt.id,
            &pjson(&rt),
        )
        .await?;
        tx.commit().await?;
        Ok(rt)
    }

    /// A runtime by id.
    pub async fn get_runtime(&self, cx: &Cx, id: &str) -> Result<AgentRuntime> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rt, _) = Self::load_runtime(&mut tx, id).await?;
        Ok(rt)
    }

    pub(crate) async fn load_runtime(tx: &mut Tx, id: &str) -> Result<(AgentRuntime, Row)> {
        let scope = locate(tx, "runtime", id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("runtime {id}")))?;
        tx.get_msg::<AgentRuntime>(&RUNTIMES, &scope, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("runtime {id}")))
    }

    /// Runtimes of an org (plus public ones), filtered by capability, paged by id.
    pub async fn list_runtimes(
        &self,
        cx: &Cx,
        org_id: &str,
        capability: &str,
        include_public: bool,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<AgentRuntime>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut all: Vec<AgentRuntime> = Vec::new();
        let mut scopes = vec![org_id.to_string()];
        if include_public && !org_id.is_empty() {
            scopes.push(String::new());
        }
        for s in scopes {
            for (rt, _) in tx
                .scan_msgs::<AgentRuntime>(&RUNTIMES, &Scan::scope(s, crate::store::MAX_SCAN))
                .await?
            {
                if capability.is_empty() || rt.capabilities.iter().any(|c| c == capability) {
                    all.push(rt);
                }
            }
        }
        all.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(page_by_id(all, after, limit, |r| r.id.clone()))
    }

    /// Updates a runtime.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_runtime(
        &self,
        cx: &Cx,
        id: &str,
        version: Option<String>,
        description: Option<String>,
        capabilities: Vec<String>,
        mcp_endpoint: Option<String>,
        image: Option<String>,
        sandbox: Option<AgentSandboxRequirements>,
        enabled: Option<bool>,
    ) -> Result<AgentRuntime> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rt, row) = Self::load_runtime(&mut tx, id).await?;
        let before = pjson(&rt);
        if let Some(v) = version {
            check_len("version", &v, 64)?;
            let key = runtime_key(&rt.name, &v);
            if let Some(other) = tx
                .scan(
                    &RUNTIMES,
                    &Scan::scope(row.scope.clone(), 1).key(Key::K1(key)),
                )
                .await?
                .first()
                && other.id != rt.id
            {
                return Err(ForgeError::already_exists(format!(
                    "runtime {}@{}",
                    rt.name, v
                )));
            }
            rt.version = v.trim().to_string();
        }
        if let Some(d) = description {
            check_len("description", &d, 4096)?;
            rt.description = d;
        }
        if !capabilities.is_empty() {
            rt.capabilities = capabilities;
        }
        if let Some(m) = mcp_endpoint {
            rt.mcp_endpoint = m;
        }
        if let Some(i) = image {
            rt.image = i;
        }
        if let Some(s) = sandbox {
            rt.sandbox = Some(s);
        }
        if let Some(e) = enabled {
            rt.enabled = e;
        }
        rt.updated_at = Some(ts(tx.now_ms()));
        tx.update(
            &RUNTIMES,
            &Row::new(row.scope.clone(), rt.id.clone())
                .k1(runtime_key(&rt.name, &rt.version))
                .n1(row.n1.unwrap_or(0))
                .version(row.version)
                .msg(&rt),
        )
        .await?;
        let org = row.scope.clone();
        self.record_org(
            &mut tx,
            &org,
            "agent_runtime.updated",
            "agent.runtime_update",
            id,
            &pjson(&rt),
        )
        .await?;
        tx.audit(
            AuditDraft::new("agent.runtime_update_detail", &org)
                .target_id(id)
                .before(&before)
                .after(&pjson(&rt)),
        )
        .await?;
        tx.commit().await?;
        Ok(rt)
    }

    /// Deletes a runtime; `INVALID_STATE` while runs using it are active.
    pub async fn delete_runtime(&self, cx: &Cx, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rt, row) = Self::load_runtime(&mut tx, id).await?;
        let active = tx
            .scan_msgs::<RunRec>(
                &RUNS,
                &Scan {
                    key: Some(Key::K2(id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
            .iter()
            .filter(|(r, _)| is_active(r.run().state))
            .count();
        if active > 0 {
            return Err(ForgeError::invalid_state(format!(
                "{active} active run(s) use runtime {}",
                rt.name
            )));
        }
        tx.delete(&RUNTIMES, &row.scope, id).await?;
        tx.delete(&LOCATOR, "runtime", id).await?;
        let org = row.scope.clone();
        self.record_org(
            &mut tx,
            &org,
            "agent_runtime.deleted",
            "agent.runtime_delete",
            id,
            &pjson(&rt),
        )
        .await?;
        tx.commit().await
    }

    // ---- definitions ----

    /// Defines an agent in an org. The principal must be an agent; its owner (when the
    /// directory knows one) must be among the owners.
    pub async fn define_agent(&self, cx: &Cx, def: AgentDefinition) -> Result<AgentDefinition> {
        let name = check_name("name", &def.name)?;
        let agent = self.agent_principal(&def.agent.id).await?;
        let toolsets = check_toolsets(&def.toolsets)?;
        check_path("instructions_path", &def.instructions_path)?;
        check_len("purpose", &def.purpose, 4096)?;
        let mut owners: Vec<String> = Vec::new();
        for o in def.owners.iter().map(|o| o.trim().to_string()) {
            if !o.is_empty() && !owners.contains(&o) {
                owners.push(o);
            }
        }
        if owners.is_empty() {
            if agent.owner_id.is_empty() {
                return Err(ForgeError::missing("owners"));
            }
            owners.push(agent.owner_id.clone());
        }
        if !agent.owner_id.is_empty() && !owners.contains(&agent.owner_id) {
            return Err(ForgeError::invalid(
                "invalid_owner",
                format!("the agent's owner {} must be an owner", agent.owner_id),
            ));
        }
        if owners.contains(&agent.id) {
            return Err(ForgeError::invalid(
                "invalid_owner",
                "an agent cannot own itself",
            ));
        }
        let schedules = self.check_schedules(&def.schedules, &[])?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (runtime_id, runtime) = self
            .runtime_of(&mut tx, &def.org_id, &def.runtime_id, &def.runtime)
            .await?;
        if tx.get(&DEFS, &def.org_id, &agent.id).await?.is_some() {
            return Err(ForgeError::already_exists(format!(
                "agent {} in this org",
                agent.id
            )));
        }
        if !tx
            .scan(
                &DEFS,
                &Scan::scope(def.org_id.clone(), 1).key(Key::K1(lc(&name))),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("agent named {name}")));
        }
        if tx.count(&DEFS, &def.org_id, None).await? as usize >= MAX_DEFINITIONS {
            return Err(ForgeError::too_large(
                "agents",
                MAX_DEFINITIONS as u64,
                MAX_DEFINITIONS as u64 + 1,
            ));
        }
        let now = tx.now_ms();
        let mut sched = Vec::new();
        for s in schedules {
            sched.push(ScheduleRec {
                id: if s.id.is_empty() { tx.new_id()? } else { s.id },
                repo_id: s.repo_id,
                cron: s.cron,
                prompt: s.prompt,
                last_fired_ms: now,
            });
        }
        let rec = DefRec {
            org_id: def.org_id.clone(),
            agent: Some(Principal {
                runtime: runtime.clone(),
                owner_id: owners[0].clone(),
                ..agent.clone()
            }),
            name: name.clone(),
            runtime_id,
            runtime,
            model: def.model.trim().to_string(),
            instructions_path: def.instructions_path.trim().to_string(),
            toolsets,
            default_budget: Some(def.default_budget),
            owners,
            purpose: def.purpose.clone(),
            enabled: def.enabled,
            created_ms: now,
            updated_ms: now,
            schedules: sched,
        };
        tx.insert(
            &DEFS,
            &Row::new(def.org_id.clone(), agent.id.clone())
                .k1(lc(&name))
                .k2(agent.id.clone())
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        let out = AgentDefinition::from_rec(&rec);
        self.record_org(
            &mut tx,
            &def.org_id,
            "agent.defined",
            "agent.define",
            &agent.id,
            &def_json(&out),
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    fn check_schedules(&self, s: &[Schedule], _old: &[ScheduleRec]) -> Result<Vec<Schedule>> {
        if s.len() > MAX_SCHEDULES {
            return Err(ForgeError::too_large(
                "schedules",
                MAX_SCHEDULES as u64,
                s.len() as u64,
            ));
        }
        for x in s {
            super::cron::Cron::parse(&x.cron)?;
            if x.repo_id.is_empty() {
                return Err(ForgeError::missing("schedules.repo_id"));
            }
            check_len("schedules.prompt", &x.prompt, 16 * 1024)?;
        }
        Ok(s.to_vec())
    }

    async fn runtime_of(
        &self,
        tx: &mut Tx,
        org_id: &str,
        runtime_id: &str,
        runtime: &str,
    ) -> Result<(String, String)> {
        if !runtime_id.is_empty() {
            let (rt, row) = Self::load_runtime(tx, runtime_id).await?;
            if !row.scope.is_empty() && row.scope != org_id {
                return Err(ForgeError::not_found(format!("runtime {runtime_id}")));
            }
            return Ok((rt.id, rt.name));
        }
        Ok((String::new(), check_runtime_name(runtime)?))
    }

    async fn agent_principal(&self, id: &str) -> Result<Principal> {
        if id.is_empty() {
            return Err(ForgeError::missing("agent"));
        }
        let p = self
            .forge
            .hooks
            .directory
            .get(id)
            .await
            .ok_or_else(|| ForgeError::not_found(format!("principal {id}")))?;
        if p.kind != PrincipalKind::Agent as i32 {
            return Err(ForgeError::invalid(
                "not_an_agent",
                format!("{id} is not an agent principal"),
            ));
        }
        Ok(p)
    }

    /// Updates a definition.
    pub async fn update_agent(
        &self,
        cx: &Cx,
        org_id: &str,
        agent_id: &str,
        patch: DefinitionPatch,
    ) -> Result<AgentDefinition> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = tx
            .get_msg::<DefRec>(&DEFS, org_id, agent_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("agent {agent_id}")))?;
        let before = def_json(&AgentDefinition::from_rec(&rec));
        if let Some(n) = patch.name {
            let n = check_name("name", &n)?;
            if let Some(other) = tx
                .scan(&DEFS, &Scan::scope(org_id, 1).key(Key::K1(lc(&n))))
                .await?
                .first()
                && other.id != agent_id
            {
                return Err(ForgeError::already_exists(format!("agent named {n}")));
            }
            rec.name = n;
        }
        if patch.runtime_id.is_some() || patch.runtime.is_some() {
            let (id, name) = self
                .runtime_of(
                    &mut tx,
                    org_id,
                    patch.runtime_id.as_deref().unwrap_or(""),
                    patch.runtime.as_deref().unwrap_or(&rec.runtime),
                )
                .await?;
            rec.runtime_id = id;
            rec.runtime = name.clone();
            if let Some(a) = rec.agent.as_mut() {
                a.runtime = name;
            }
        }
        if let Some(m) = patch.model {
            rec.model = m.trim().to_string();
        }
        if let Some(p) = patch.instructions_path {
            check_path("instructions_path", &p)?;
            rec.instructions_path = p.trim().to_string();
        }
        if let Some(t) = patch.toolsets {
            rec.toolsets = check_toolsets(&t)?;
        }
        if let Some(b) = patch.default_budget {
            rec.default_budget = Some(b);
        }
        if let Some(o) = patch.owners {
            let mut owners: Vec<String> = Vec::new();
            for x in o.iter().map(|x| x.trim().to_string()) {
                if !x.is_empty() && !owners.contains(&x) && x != agent_id {
                    owners.push(x);
                }
            }
            if owners.is_empty() {
                return Err(ForgeError::missing("owners"));
            }
            if let Some(a) = rec.agent.as_mut() {
                a.owner_id = owners[0].clone();
            }
            rec.owners = owners;
        }
        if let Some(p) = patch.purpose {
            check_len("purpose", &p, 4096)?;
            rec.purpose = p;
        }
        if let Some(e) = patch.enabled {
            rec.enabled = e;
        }
        if let Some(s) = patch.schedules {
            let s = self.check_schedules(&s, &rec.schedules)?;
            let now = tx.now_ms();
            let mut out = Vec::new();
            for x in s {
                let last = rec
                    .schedules
                    .iter()
                    .find(|o| !x.id.is_empty() && o.id == x.id)
                    .map(|o| o.last_fired_ms)
                    .unwrap_or(now);
                out.push(ScheduleRec {
                    id: if x.id.is_empty() { tx.new_id()? } else { x.id },
                    repo_id: x.repo_id,
                    cron: x.cron,
                    prompt: x.prompt,
                    last_fired_ms: last,
                });
            }
            rec.schedules = out;
        }
        rec.updated_ms = tx.now_ms();
        tx.update(
            &DEFS,
            &Row::new(org_id, agent_id)
                .k1(lc(&rec.name))
                .k2(agent_id)
                .n1(row.n1.unwrap_or(0))
                .version(row.version)
                .msg(&rec),
        )
        .await?;
        let out = AgentDefinition::from_rec(&rec);
        tx.emit_event(org_event(
            org_id,
            "agent.updated",
            agent_id,
            &def_json(&out),
        ))
        .await?;
        tx.audit(
            AuditDraft::new("agent.update", org_id)
                .target_id(agent_id)
                .before(&before)
                .after(&def_json(&out)),
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Deletes a definition (the principal stays; it no longer takes work).
    pub async fn delete_agent(&self, cx: &Cx, org_id: &str, agent_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = tx
            .get_msg::<DefRec>(&DEFS, org_id, agent_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("agent {agent_id}")))?;
        tx.delete(&DEFS, org_id, agent_id).await?;
        let json = def_json(&AgentDefinition::from_rec(&rec));
        self.record_org(
            &mut tx,
            org_id,
            "agent.deleted",
            "agent.delete",
            agent_id,
            &json,
        )
        .await?;
        tx.commit().await
    }

    /// The definition of `agent_id` in `org_id`.
    pub async fn definition(
        &self,
        cx: &Cx,
        org_id: &str,
        agent_id: &str,
    ) -> Result<AgentDefinition> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<DefRec>(&DEFS, org_id, agent_id)
            .await?
            .map(|(r, _)| AgentDefinition::from_rec(&r))
            .ok_or_else(|| ForgeError::not_found(format!("agent {agent_id}")))
    }

    /// Definitions of an org, by name.
    pub async fn definitions(&self, cx: &Cx, org_id: &str) -> Result<Vec<AgentDefinition>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut v: Vec<AgentDefinition> = tx
            .scan_msgs::<DefRec>(&DEFS, &Scan::scope(org_id, crate::store::MAX_SCAN))
            .await?
            .iter()
            .map(|(r, _)| AgentDefinition::from_rec(r))
            .collect();
        v.sort_by_key(|d| lc(&d.name));
        Ok(v)
    }

    /// Every org definition of one agent principal.
    pub async fn definitions_of(&self, cx: &Cx, agent_id: &str) -> Result<Vec<AgentDefinition>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<DefRec>(
                &DEFS,
                &Scan {
                    key: Some(Key::K2(agent_id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
            .iter()
            .map(|(r, _)| AgentDefinition::from_rec(r))
            .collect())
    }

    // ---- budgets ----

    /// Sets an owner's monthly budget in an org.
    pub async fn set_owner_budget(
        &self,
        cx: &Cx,
        org_id: &str,
        owner_id: &str,
        b: OwnerBudget,
    ) -> Result<OwnerBudget> {
        if owner_id.is_empty() {
            return Err(ForgeError::missing("owner"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rec = OwnerBudgetRec {
            owner_id: owner_id.to_string(),
            max_usd_micros: b.max_usd_micros,
            max_input_tokens: b.max_input_tokens,
            max_output_tokens: b.max_output_tokens,
            max_runs: b.max_runs,
            updated_ms: tx.now_ms(),
        };
        tx.put(&OWNER_BUDGETS, &Row::new(org_id, owner_id).msg(&rec))
            .await?;
        let json = serde_json::json!({
            "owner_id": owner_id, "max_usd_micros": b.max_usd_micros,
            "max_input_tokens": b.max_input_tokens, "max_output_tokens": b.max_output_tokens,
            "max_runs": b.max_runs,
        })
        .to_string();
        self.record_org(
            &mut tx,
            org_id,
            "agent.budget_updated",
            "agent.budget_set",
            owner_id,
            &json,
        )
        .await?;
        tx.commit().await?;
        Ok(b)
    }

    /// An owner's budget and this month's spend.
    pub async fn owner_budget(
        &self,
        cx: &Cx,
        org_id: &str,
        owner_id: &str,
    ) -> Result<(OwnerBudget, OwnerSpend)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let b = tx
            .get_msg::<OwnerBudgetRec>(&OWNER_BUDGETS, org_id, owner_id)
            .await?
            .map(|(b, _)| OwnerBudget {
                max_usd_micros: b.max_usd_micros,
                max_input_tokens: b.max_input_tokens,
                max_output_tokens: b.max_output_tokens,
                max_runs: b.max_runs,
            })
            .unwrap_or_default();
        let s = tx
            .get_msg::<SpendRec>(
                &SPEND,
                org_id,
                &format!("{owner_id}/{}", utc_month(tx.now_ms())),
            )
            .await?
            .map(|(s, _)| OwnerSpend {
                usd_micros: s.usd_micros,
                input_tokens: s.input_tokens,
                output_tokens: s.output_tokens,
                runs: s.runs,
            })
            .unwrap_or_default();
        Ok((b, s))
    }

    /// `Ok` when the owner may start another run, else `BUDGET_EXHAUSTED`.
    pub async fn check_owner_budget(&self, cx: &Cx, org_id: &str, owner_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        ensure_budget(&mut tx, org_id, owner_id).await
    }

    // ---- principals, dashboards, work items ----

    /// Summaries of the agents defined in an org (filtered by owner, runtime and ids), with
    /// stats since `window_start_ms`.
    pub async fn principal_summaries(
        &self,
        cx: &Cx,
        org_id: &str,
        owner_id: &str,
        runtime: &str,
        ids: &[String],
        window_start_ms: u64,
    ) -> Result<Vec<AgentPrincipalSummary>> {
        let defs = self.definitions(cx, org_id).await?;
        let mut out = Vec::new();
        for d in defs {
            if !owner_id.is_empty() && !d.owners.iter().any(|o| o == owner_id) {
                continue;
            }
            if !runtime.is_empty() && d.runtime != runtime && d.runtime_id != runtime {
                continue;
            }
            if !ids.is_empty() && !ids.contains(&d.agent.id) {
                continue;
            }
            let facts = self.facts(cx, &d.agent.id, org_id).await?;
            let stats = facts.stats(window_start_ms, u64::MAX, "");
            let active_leases = self.active_leases(cx, &d.agent.id, &facts).await?;
            let last = facts
                .sessions
                .iter()
                .max_by_key(|s| (s.created_ms, s.session.clone()));
            let last_active = facts.last_active_ms();
            out.push(AgentPrincipalSummary {
                principal: Some(d.agent.clone()),
                purpose: d.purpose.clone(),
                stats: Some(AgentStats {
                    active_leases,
                    ..stats
                }),
                last_active_at: (last_active > 0).then(|| ts(last_active)),
                last_model: last.map(|s| s.model.clone()).unwrap_or_default(),
                last_session: last.map(|s| s.session.clone()).unwrap_or_default(),
                open_change_requests: facts.crs.iter().filter(|c| c.state == "open").count() as u64,
            });
        }
        Ok(out)
    }

    async fn active_leases(&self, cx: &Cx, agent_id: &str, f: &Facts) -> Result<u64> {
        let mut repos: BTreeSet<String> = BTreeSet::new();
        for r in &f.runs {
            if let Some(id) = r.run().repo.and_then(|r| r.repo) {
                match id {
                    repo_ref::Repo::Id(i) | repo_ref::Repo::FullName(i) => {
                        repos.insert(i);
                    }
                }
            }
        }
        for c in &f.crs {
            repos.insert(c.repo_id.clone());
        }
        let mut n = 0u64;
        for repo in repos.into_iter().take(50) {
            if let Ok(leases) = self.forge.leases.list(cx, &repo, &[]).await {
                n += leases
                    .iter()
                    .filter(|l| l.holder.as_ref().is_some_and(|h| h.id == agent_id))
                    .count() as u64;
            }
        }
        Ok(n)
    }

    /// One agent's dashboard over `[start_ms, end_ms)` (0 / `u64::MAX` = unbounded), limited
    /// to `repo_id` when set.
    pub async fn dashboard(
        &self,
        cx: &Cx,
        agent: &Principal,
        org_ids: &[String],
        start_ms: u64,
        end_ms: u64,
        repo_id: &str,
    ) -> Result<AgentDashboard> {
        let mut facts = Facts::default();
        for org in org_ids {
            facts.merge(self.facts(cx, &agent.id, org).await?);
        }
        let totals = facts.stats(start_ms, end_ms, repo_id);
        let mut days: BTreeSet<String> = BTreeSet::new();
        for t in facts.times(start_ms, end_ms, repo_id) {
            days.insert(utc_date(t));
        }
        let mut daily = Vec::new();
        for d in days {
            let (s, e) = day_bounds(&d);
            daily.push(AgentDailyStats {
                date: d,
                stats: Some(facts.stats(s.max(start_ms), e.min(end_ms), repo_id)),
            });
        }
        let mut repo_counts: BTreeMap<String, u64> = BTreeMap::new();
        for (repo, t) in facts.repo_times() {
            if t >= start_ms && t < end_ms {
                *repo_counts.entry(repo).or_default() += 1;
            }
        }
        let mut top: Vec<(String, u64)> = repo_counts.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut models: Vec<String> = facts
            .sessions
            .iter()
            .filter(|s| s.created_ms >= start_ms && s.created_ms < end_ms)
            .map(|s| s.model.clone())
            .filter(|m| !m.is_empty())
            .collect();
        models.sort();
        models.dedup();
        Ok(AgentDashboard {
            principal: Some(agent.clone()),
            on_behalf_of: self.owner_chain(cx, agent).await?,
            totals: Some(totals),
            daily,
            top_repo_ids: top.into_iter().take(10).map(|(r, _)| r).collect(),
            models,
            window_start: (start_ms > 0).then(|| ts(start_ms)),
            window_end: (end_ms < u64::MAX).then(|| ts(end_ms)),
        })
    }

    /// The delegation chain behind an agent: its owner, the owner's owner (for agents owning
    /// agents), up to a human or service root.
    pub async fn owner_chain(&self, cx: &Cx, agent: &Principal) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        let mut cur = agent.clone();
        for _ in 0..16 {
            let mut owner = cur.owner_id.clone();
            if owner.is_empty() {
                owner = self
                    .definitions_of(cx, &cur.id)
                    .await?
                    .first()
                    .map(|d| d.owner().to_string())
                    .unwrap_or_default();
            }
            if owner.is_empty() || out.contains(&owner) || owner == agent.id {
                break;
            }
            out.push(owner.clone());
            match self.forge.hooks.directory.get(&owner).await {
                Some(p) if p.kind == PrincipalKind::Agent as i32 => cur = p,
                _ => break,
            }
        }
        Ok(out)
    }

    /// An agent's work items (runs, change requests, reviews, changesets), newest first,
    /// paged by offset.
    #[allow(clippy::too_many_arguments)]
    pub async fn work_items(
        &self,
        cx: &Cx,
        agent_id: &str,
        org_ids: &[String],
        repo_id: &str,
        since_ms: u64,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<AgentWorkItem>, String)> {
        let mut facts = Facts::default();
        for org in org_ids {
            facts.merge(self.facts(cx, agent_id, org).await?);
        }
        let mut items: Vec<(u64, String, AgentWorkItem)> = Vec::new();
        let keep = |repo: &str, t: u64| (repo_id.is_empty() || repo == repo_id) && t >= since_ms;
        for r in &facts.runs {
            let run = r.run();
            let repo = run_repo(&run);
            let t = run.created_at.as_ref().map(ts_ms).unwrap_or(0);
            if keep(&repo, t) {
                items.push((
                    t,
                    run.id.clone(),
                    AgentWorkItem {
                        subject: Some(SubjectRef {
                            kind: SubjectKind::AgentRun as i32,
                            repo: repo_ref_id(&repo),
                            id: run.id.clone(),
                        }),
                        title: run_title(&run),
                        state: state_name(run.state),
                        cost: run.cost,
                        session: run.session.clone(),
                        created_at: run.created_at,
                    },
                ));
            }
        }
        for c in &facts.crs {
            if keep(&c.repo_id, c.created_ms) {
                items.push((
                    c.created_ms,
                    c.cr_id.clone(),
                    AgentWorkItem {
                        subject: Some(SubjectRef {
                            kind: SubjectKind::ChangeRequest as i32,
                            repo: repo_ref_id(&c.repo_id),
                            id: c.number.to_string(),
                        }),
                        title: c.title.clone(),
                        state: if c.reverted {
                            "reverted".into()
                        } else {
                            c.state.clone()
                        },
                        cost: None,
                        session: c.session.clone(),
                        created_at: Some(ts(c.created_ms)),
                    },
                ));
            }
        }
        for (id, r) in &facts.reviews {
            if keep(&r.repo_id, r.created_ms) {
                items.push((
                    r.created_ms,
                    id.clone(),
                    AgentWorkItem {
                        subject: Some(SubjectRef {
                            kind: SubjectKind::ChangeRequest as i32,
                            repo: repo_ref_id(&r.repo_id),
                            id: r.cr_id.clone(),
                        }),
                        title: format!("review: {}", r.verdict.to_lowercase()),
                        state: "submitted".into(),
                        cost: None,
                        session: String::new(),
                        created_at: Some(ts(r.created_ms)),
                    },
                ));
            }
        }
        for s in &facts.sessions {
            for cs in &s.changesets {
                if keep(&s.repo_id, s.created_ms) {
                    items.push((
                        s.created_ms,
                        format!("{}:{}", s.session, hex(&cs.raw)),
                        AgentWorkItem {
                            subject: Some(SubjectRef {
                                kind: SubjectKind::Changeset as i32,
                                repo: repo_ref_id(&s.repo_id),
                                id: hex(&cs.raw),
                            }),
                            title: format!(
                                "changeset {}",
                                &hex(&cs.raw)[..12.min(cs.raw.len() * 2)]
                            ),
                            state: "created".into(),
                            cost: None,
                            session: s.session.clone(),
                            created_at: Some(ts(s.created_ms)),
                        },
                    ));
                }
            }
        }
        items.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let start: usize = after.parse().unwrap_or(0);
        let end = (start + limit).min(items.len());
        let next = if end < items.len() {
            end.to_string()
        } else {
            String::new()
        };
        Ok((
            items
                .into_iter()
                .skip(start)
                .take(end.saturating_sub(start))
                .map(|(_, _, i)| i)
                .collect(),
            next,
        ))
    }

    /// The agent activity stream: events of an org log acted by an agent or on behalf of a
    /// delegation root. Returns the events and the sequence to resume from.
    pub async fn activity(&self, cx: &Cx, f: &ActivityFilter) -> Result<(Vec<Event>, u64)> {
        let limit = if f.limit == 0 { 100 } else { f.limit.min(1000) };
        let log = crate::events::org_log(&f.org_id);
        let mut from = f.from_seq;
        let mut out = Vec::new();
        for _ in 0..100 {
            let (events, next) = self
                .forge
                .events
                .read(cx, &log, from, &f.kinds, 1000)
                .await?;
            if events.is_empty() {
                return Ok((out, from));
            }
            for e in events {
                let seq = e.seq;
                let by_agent = f.agent_id.is_empty() || e.principal == f.agent_id;
                let root = e.on_behalf_of.last().unwrap_or(&e.principal);
                let by_root = f.delegation_root.is_empty() || *root == f.delegation_root;
                if by_agent && by_root {
                    out.push(e);
                    if out.len() >= limit {
                        return Ok((out, seq + 1));
                    }
                }
                from = seq + 1;
            }
            if next <= from && next != 0 {
                from = next.max(from);
            }
        }
        Ok((out, from))
    }

    pub(crate) async fn facts(&self, cx: &Cx, agent_id: &str, org_id: &str) -> Result<Facts> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let runs: Vec<RunRec> = tx
            .scan_msgs::<RunRec>(
                &RUNS,
                &Scan {
                    key: Some(Key::K1(agent_id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
            .into_iter()
            .map(|(r, _)| r)
            .filter(|r| org_id.is_empty() || r.org_id == org_id)
            .collect();
        let sessions: Vec<SessionRec> = tx
            .scan_msgs::<SessionRec>(
                &SESSIONS,
                &Scan {
                    key: Some(Key::K1(agent_id.to_string())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
            .into_iter()
            .map(|(r, _)| r)
            .filter(|s| org_id.is_empty() || s.org_id == org_id)
            .collect();
        let crs: Vec<AgentCrRec> = tx
            .scan_msgs::<AgentCrRec>(&AGENT_CRS, &Scan::scope(agent_id, crate::store::MAX_SCAN))
            .await?
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        let reviews: Vec<(String, AgentReviewRec)> = tx
            .scan_msgs::<AgentReviewRec>(
                &AGENT_REVIEWS,
                &Scan::scope(agent_id, crate::store::MAX_SCAN),
            )
            .await?
            .into_iter()
            .map(|(r, row)| (row.id, r))
            .collect();
        let mut f = Facts {
            runs,
            sessions,
            crs,
            reviews,
        };
        if !org_id.is_empty() {
            // CRs and reviews are kept per agent; keep the ones in repos of this org's runs
            // and sessions, plus every repo of the org the orgs module knows.
            let mut repos: BTreeSet<String> = BTreeSet::new();
            for r in &f.runs {
                repos.insert(run_repo(&r.run()));
            }
            for s in &f.sessions {
                repos.insert(s.repo_id.clone());
            }
            let keep_repo = |id: &str| repos.contains(id);
            let mut cache: BTreeMap<String, bool> = BTreeMap::new();
            let mut crs = Vec::new();
            for c in std::mem::take(&mut f.crs) {
                let ok = keep_repo(&c.repo_id) || {
                    match cache.get(&c.repo_id) {
                        Some(v) => *v,
                        None => {
                            let v = self
                                .forge
                                .orgs
                                .repo_any(cx, &c.repo_id)
                                .await
                                .map(|r| r.org_id == org_id)
                                .unwrap_or(false);
                            cache.insert(c.repo_id.clone(), v);
                            v
                        }
                    }
                };
                if ok {
                    crs.push(c);
                }
            }
            f.crs = crs;
            let mut reviews = Vec::new();
            for (id, r) in std::mem::take(&mut f.reviews) {
                let ok = keep_repo(&r.repo_id) || {
                    match cache.get(&r.repo_id) {
                        Some(v) => *v,
                        None => {
                            let v = self
                                .forge
                                .orgs
                                .repo_any(cx, &r.repo_id)
                                .await
                                .map(|x| x.org_id == org_id)
                                .unwrap_or(false);
                            cache.insert(r.repo_id.clone(), v);
                            v
                        }
                    }
                };
                if ok {
                    reviews.push((id, r));
                }
            }
            f.reviews = reviews;
        }
        Ok(f)
    }

    /// Records an org-scoped change: an event and an audit entry.
    pub(crate) async fn record_org(
        &self,
        tx: &mut Tx,
        org_id: &str,
        kind: &str,
        action: &str,
        target: &str,
        json: &str,
    ) -> Result<()> {
        if !org_id.is_empty() {
            tx.emit_event(org_event(org_id, kind, target, json)).await?;
        }
        tx.audit(
            AuditDraft::new(action, org_id)
                .target_id(target)
                .after(json),
        )
        .await
    }
}

/// An org-log event with `payload_json`.
pub(crate) fn org_event(org_id: &str, kind: &str, target: &str, json: &str) -> Event {
    Event {
        kind: kind.to_string(),
        org_id: org_id.to_string(),
        subject: Some(SubjectRef {
            kind: SubjectKind::Org as i32,
            repo: None,
            id: target.to_string(),
        }),
        payload_json: json.to_string(),
        ..Default::default()
    }
}

pub(crate) fn def_json(d: &AgentDefinition) -> String {
    serde_json::json!({
        "org_id": d.org_id,
        "agent": d.agent.id,
        "name": d.name,
        "runtime_id": d.runtime_id,
        "runtime": d.runtime,
        "model": d.model,
        "instructions_path": d.instructions_path,
        "toolsets": d.toolsets,
        "default_budget": serde_json::from_str::<serde_json::Value>(&pjson(&d.default_budget)).unwrap_or_default(),
        "owners": d.owners,
        "purpose": d.purpose,
        "enabled": d.enabled,
        "schedules": d.schedules.iter().map(|s| serde_json::json!({"id": s.id, "repo_id": s.repo_id, "cron": s.cron})).collect::<Vec<_>>(),
    })
    .to_string()
}

pub(crate) fn run_repo(run: &AgentRun) -> String {
    run_repo_ref(&run.repo)
}

/// The id (or name) a repo reference carries.
pub(crate) fn run_repo_ref(r: &Option<RepoRef>) -> String {
    match r.as_ref().and_then(|r| r.repo.as_ref()) {
        Some(repo_ref::Repo::Id(i)) | Some(repo_ref::Repo::FullName(i)) => i.clone(),
        None => String::new(),
    }
}

pub(crate) fn state_name(s: i32) -> String {
    AgentRunState::try_from(s)
        .unwrap_or(AgentRunState::Unspecified)
        .as_str_name()
        .trim_start_matches("AGENT_RUN_STATE_")
        .to_lowercase()
}

pub(crate) fn run_title(run: &AgentRun) -> String {
    let t = run.task.clone().unwrap_or_default();
    let kind = bee_proto::v1::AgentTaskKind::try_from(t.kind)
        .unwrap_or(bee_proto::v1::AgentTaskKind::Unspecified);
    match kind {
        bee_proto::v1::AgentTaskKind::Issue => format!("issue #{}", t.issue),
        bee_proto::v1::AgentTaskKind::Review => format!("review #{}", t.change_request),
        bee_proto::v1::AgentTaskKind::Revise => format!("revise #{}", t.change_request),
        bee_proto::v1::AgentTaskKind::SecurityFix => "security fix".into(),
        bee_proto::v1::AgentTaskKind::AdoptEvaluate => format!("adopt post {}", t.post_id),
        _ => {
            let p: String = t.prompt.chars().take(80).collect();
            if p.is_empty() { "task".into() } else { p }
        }
    }
}

fn day_bounds(date: &str) -> (u64, u64) {
    // Linear scan from the epoch is avoided: parse the date and count days.
    let parts: Vec<i64> = date.split('-').filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 3 {
        return (0, u64::MAX);
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let start = (days.max(0) as u64) * 86_400_000;
    (start, start + 86_400_000)
}

fn page_by_id<T>(
    all: Vec<T>,
    after: &str,
    limit: usize,
    id: impl Fn(&T) -> String,
) -> (Vec<T>, String) {
    let mut v: Vec<T> = all
        .into_iter()
        .filter(|x| after.is_empty() || id(x).as_str() > after)
        .collect();
    let more = v.len() > limit;
    v.truncate(limit);
    let next = if more {
        v.last().map(&id).unwrap_or_default()
    } else {
        String::new()
    };
    (v, next)
}

/// Everything known about one agent's work.
#[derive(Default)]
pub(crate) struct Facts {
    pub runs: Vec<RunRec>,
    pub sessions: Vec<SessionRec>,
    pub crs: Vec<AgentCrRec>,
    pub reviews: Vec<(String, AgentReviewRec)>,
}

impl Facts {
    fn merge(&mut self, o: Facts) {
        for r in o.runs {
            if !self.runs.iter().any(|x| x.run().id == r.run().id) {
                self.runs.push(r);
            }
        }
        for s in o.sessions {
            if !self.sessions.iter().any(|x| x.session == s.session) {
                self.sessions.push(s);
            }
        }
        for c in o.crs {
            if !self.crs.iter().any(|x| x.cr_id == c.cr_id) {
                self.crs.push(c);
            }
        }
        for r in o.reviews {
            if !self.reviews.iter().any(|x| x.0 == r.0) {
                self.reviews.push(r);
            }
        }
    }

    fn last_active_ms(&self) -> u64 {
        let mut t = 0;
        for r in &self.runs {
            let run = r.run();
            for x in [&run.created_at, &run.started_at, &run.finished_at]
                .into_iter()
                .flatten()
            {
                t = t.max(ts_ms(x));
            }
        }
        for c in &self.crs {
            t = t.max(c.created_ms);
        }
        for (_, r) in &self.reviews {
            t = t.max(r.created_ms);
        }
        t
    }

    fn times(&self, s: u64, e: u64, repo: &str) -> Vec<u64> {
        let mut v = Vec::new();
        for (r, t) in self.repo_times() {
            if (repo.is_empty() || r == repo) && t >= s && t < e {
                v.push(t);
            }
        }
        v
    }

    fn repo_times(&self) -> Vec<(String, u64)> {
        let mut v = Vec::new();
        for r in &self.runs {
            let run = r.run();
            v.push((
                run_repo(&run),
                run.created_at.as_ref().map(ts_ms).unwrap_or(0),
            ));
        }
        for c in &self.crs {
            v.push((c.repo_id.clone(), c.created_ms));
        }
        for (_, r) in &self.reviews {
            v.push((r.repo_id.clone(), r.created_ms));
        }
        for s in &self.sessions {
            v.push((s.repo_id.clone(), s.created_ms));
        }
        v
    }

    fn stats(&self, s: u64, e: u64, repo: &str) -> AgentStats {
        let inw = |r: &str, t: u64| (repo.is_empty() || r == repo) && t >= s && t < e;
        let mut cost = Cost::default();
        let mut out = AgentOutcomes::default();
        let mut work = 0u64;
        let mut active = 0u64;
        for r in &self.runs {
            let run = r.run();
            let t = run.created_at.as_ref().map(ts_ms).unwrap_or(0);
            if is_active(run.state) && (repo.is_empty() || run_repo(&run) == repo) {
                active += 1;
            }
            if !inw(&run_repo(&run), t) {
                continue;
            }
            work += 1;
            if let Some(c) = &run.cost {
                add_cost(&mut cost, c);
            }
            match AgentRunState::try_from(run.state).unwrap_or(AgentRunState::Unspecified) {
                AgentRunState::Succeeded => out.runs_succeeded += 1,
                AgentRunState::Failed => out.runs_failed += 1,
                _ => {}
            }
        }
        for c in &self.crs {
            if !inw(&c.repo_id, c.created_ms) {
                continue;
            }
            work += 1;
            out.change_requests_opened += 1;
            match c.state.as_str() {
                "landed" => out.change_requests_landed += 1,
                "closed" => out.change_requests_closed += 1,
                _ => {}
            }
            if c.reverted {
                out.changes_reverted += 1;
            }
        }
        for (_, r) in &self.reviews {
            if inw(&r.repo_id, r.created_ms) {
                work += 1;
                out.reviews_submitted += 1;
            }
        }
        let changesets = self
            .sessions
            .iter()
            .filter(|x| inw(&x.repo_id, x.created_ms))
            .map(|x| x.changesets.len() as u64)
            .sum();
        AgentStats {
            changesets,
            work_items: work,
            cost: Some(cost),
            outcomes: Some(out),
            active_leases: 0,
            active_runs: active,
        }
    }
}
