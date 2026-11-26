//! `PolicyService` (SPEC §9.4, `docs/spec/operations.md` §8).
//!
//! - **Effective policy, validation, evaluation, epochs** over the same evaluator the write
//!   path uses ([`bee_policy::Policy`]): `EvaluatePolicyAction` builds a hypothetical land
//!   context (paths, diff class, approvals, passing checks) or runs the access and ref checks.
//! - **Rulesets** created through the API are stored in the server store and become
//!   `[[rulesets]]` entries of the effective policy of every repo they cover (repo, org or
//!   enterprise owner, with repo name and property conditions applied per repo), named
//!   `api-<id>` so they never collide with policy-file rulesets. Land therefore enforces
//!   them exactly like policy-file rulesets: `ACTIVE` blocks, `EVALUATE` is evaluated and
//!   recorded with `would_block`, bypass actors skip with a logged `BYPASSED`, and a ref an
//!   active ruleset matches is protected (it moves only through Land, so deletion,
//!   force-push and direct creation are refused). Rule types without a land-policy
//!   equivalent (deployments, workflows, code scanning, message patterns, merge methods, the
//!   merge queue, conversation resolution), ref exclusions, and role or app bypass actors are
//!   refused with `INVALID_ARGUMENT` rather than stored unenforced.
//! - **Rule insights**: every land that evaluated a ruleset is recorded (from the land
//!   coalescer's decision log); `SimulateRuleset` replays recent lands against an existing
//!   or draft ruleset.
//! - **Custom roles** are the enterprise module's ([`bee_forge::enterprise::roles`]).
//! - **CODEOWNERS** and **signature requirements** read the effective policy at a revision.

use bee_forge::safety::audit_actions; // Shared with the Safety audit consumer.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use bee_core::{ObjectId, RepoPath};
use bee_forge::auth::OrgPerm;
use bee_policy::{
    Action, Actor, ChangedPath, CheckResult, CheckStatus, CommitInfo, DiffClass, LandContext,
    LandDecision, Policy, PolicyDoc, RefAction, Review, RulesetMode, RulesetOutcome,
    SignatureState, Verdict,
};
use bee_proto::v1::policy_service_server::PolicyService;
use bee_proto::v1::{self, ErrorReason};
use bee_store::RepoId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::Caller;
use crate::convert::{id_from, id_to};
use crate::errors::{ApiError, ApiResult};
use crate::forge::forge_to_api;
use crate::policy::{POLICY_PATH, read_text};
use crate::services::Rpc;
use crate::services::support::{
    all_repos, get_json, next_page, offset_page, page_of, put_json, scan_json,
};
use crate::state::{AppState, RepoCtx, RequestMeta};
use crate::util::timestamp_ms;

/// Rulesets (`<id>`).
pub const NS_RULESET: &str = "ruleset";
/// Recorded ruleset evaluations (`<repo id>/<time, 20 digits>/<id>`).
pub const NS_EVAL: &str = "ruleset_eval";

/// A stored ruleset with its resolved owner.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RulesetRec {
    /// `repo`, `org` or `enterprise`.
    pub owner_kind: String,
    /// Repo id, org id or enterprise slug.
    pub owner_id: String,
    /// Repo full name, org name or enterprise slug.
    pub owner_name: String,
    /// The ruleset (proto3 JSON).
    pub ruleset: v1::Ruleset,
}

/// Rendered ruleset text per repo, dropped whenever a ruleset changes.
#[derive(Default)]
pub struct RulesetCache {
    by_repo: Mutex<HashMap<RepoId, (std::time::Instant, String)>>,
}

/// How long a rendered ruleset text is reused before conditions that depend on forge data
/// (repo properties, enterprise membership) are evaluated again.
const RULESET_TEXT_TTL: std::time::Duration = std::time::Duration::from_secs(5);

impl RulesetCache {
    /// The cached text, and whether it is still fresh.
    fn get(&self, repo: RepoId) -> Option<(String, bool)> {
        let m = self.by_repo.lock().ok()?;
        m.get(&repo)
            .map(|(at, t)| (t.clone(), at.elapsed() < RULESET_TEXT_TTL))
    }

    fn put(&self, repo: RepoId, text: String) {
        if let Ok(mut m) = self.by_repo.lock() {
            if m.len() > 100_000 {
                m.clear();
            }
            m.insert(repo, (std::time::Instant::now(), text));
        }
    }

    fn clear(&self) {
        if let Ok(mut m) = self.by_repo.lock() {
            m.clear();
        }
    }
}

/// The org default text of `repo` plus the API rulesets that cover it (the text the
/// effective policy is built from).
pub async fn effective_org_text(state: &AppState, repo: &RepoCtx) -> ApiResult<String> {
    let mut text = state.org_text(&state.policy_org(repo).await?).await?;
    let cached = state.rulesets.get(repo.repo.id);
    let extra = match cached {
        Some((t, true)) => t,
        stale => {
            let all = scan_json::<RulesetRec>(state.kv.as_ref(), NS_RULESET, "").await?;
            let mut applicable = Vec::new();
            for (_, r) in all {
                if covers(state, &r, repo).await {
                    applicable.push(r);
                }
            }
            let t = render(&applicable, &default_ref(state, repo).await);
            state.rulesets.put(repo.repo.id, t.clone());
            // Conditions over forge data changed what applies: cached policies are stale.
            if stale.is_some_and(|(old, _)| old != t) {
                state.policies.forget_all();
            }
            t
        }
    };
    if !extra.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&extra);
    }
    Ok(text)
}

async fn default_ref(state: &AppState, repo: &RepoCtx) -> String {
    if let Some(w) = state.hub.get()
        && let Ok(r) = w
            .forge
            .orgs
            .repo_any(&bee_forge::Cx::system(), &repo.id_text())
            .await
        && !r.default_ref.is_empty()
    {
        return r.default_ref;
    }
    "main".into()
}

/// fnmatch with `*` (any run) and `?` (one character).
fn fnmatch(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi].eq_ignore_ascii_case(&t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether a ruleset covers `repo`.
async fn covers(state: &AppState, r: &RulesetRec, repo: &RepoCtx) -> bool {
    let c = r.ruleset.conditions.clone().unwrap_or_default();
    let name_ok = |name: &str| {
        (c.repo_name_includes.is_empty()
            || c.repo_name_includes
                .iter()
                .any(|p| p == "~ALL" || fnmatch(p, name)))
            && !c.repo_name_excludes.iter().any(|p| fnmatch(p, name))
    };
    match r.owner_kind.as_str() {
        "repo" => r.owner_id == repo.id_text(),
        "org" => {
            if r.owner_id != repo.org.id.0.to_text() || !name_ok(&repo.repo.name) {
                return false;
            }
            props_ok(state, &c, repo).await
        }
        "enterprise" => {
            let Some(w) = state.hub.get() else {
                return false;
            };
            let cx = bee_forge::Cx::system();
            let in_ent = matches!(
                w.enterprise.accounts.enterprise_of_org(&cx, &repo.org.id.0.to_text()).await,
                Ok(Some(e)) if e.slug == r.owner_id || e.id == r.owner_id
            );
            in_ent
                && (c.org_name_includes.is_empty()
                    || c.org_name_includes
                        .iter()
                        .any(|p| p == "~ALL" || fnmatch(p, &repo.org.name)))
                && name_ok(&repo.repo.name)
                && props_ok(state, &c, repo).await
        }
        _ => false,
    }
}

/// A repo's custom property values for ruleset conditions (F416-020): every defined
/// property with org defaults filled in, multi-select values split one per entry, plus the
/// system property `visibility` (`public`, `private` or `internal`). Empty without a forge.
pub async fn repo_property_values(
    state: &AppState,
    repo: &RepoCtx,
) -> BTreeMap<String, Vec<String>> {
    let Some(w) = state.hub.get() else {
        return BTreeMap::new();
    };
    let cx = bee_forge::Cx::system();
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Ok(values) = w.forge.orgs.repo_properties(&cx, &repo.id_text()).await {
        for kv in values {
            out.entry(kv.key.to_lowercase()).or_default().extend(
                kv.value
                    .split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty()),
            );
        }
    }
    if let Ok(r) = w.forge.orgs.repo_any(&cx, &repo.id_text()).await {
        let vis = v1::Visibility::try_from(r.visibility)
            .map(|v| {
                v.as_str_name()
                    .trim_start_matches("VISIBILITY_")
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        out.insert("visibility".into(), vec![vis]);
    }
    out
}

async fn props_ok(state: &AppState, c: &v1::RulesetConditions, repo: &RepoCtx) -> bool {
    if c.repo_property_includes.is_empty() && c.repo_property_excludes.is_empty() {
        return true;
    }
    let props = repo_property_values(state, repo).await;
    let hit = |m: &v1::RulesetPropertyMatcher| {
        props.get(&m.name.to_lowercase()).is_some_and(|vals| {
            vals.iter()
                .any(|v| m.values.iter().any(|x| x.eq_ignore_ascii_case(v)))
        })
    };
    c.repo_property_includes.iter().all(hit) && !c.repo_property_excludes.iter().any(hit)
}

/// A ref condition as a policy ref pattern.
fn ref_pattern(raw: &str, target: i32, default_ref: &str) -> String {
    let tag = target == v1::RulesetTarget::Tag as i32;
    let mut s = match raw {
        "~ALL" => return if tag { "tags/*".into() } else { "*".into() },
        "~DEFAULT_BRANCH" => default_ref.to_string(),
        r => r.to_string(),
    };
    if let Some(rest) = s.strip_prefix("refs/heads/") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("refs/tags/") {
        s = format!("tags/{rest}");
    } else if tag && !s.starts_with("tags/") {
        s = format!("tags/{s}");
    }
    match s.find(['*', '?', '[']) {
        Some(i) => format!("{}*", &s[..i]),
        None => s,
    }
}

fn mode_of(e: i32) -> &'static str {
    match v1::RulesetEnforcement::try_from(e).unwrap_or(v1::RulesetEnforcement::Active) {
        v1::RulesetEnforcement::Evaluate => "evaluate",
        v1::RulesetEnforcement::Disabled => "disabled",
        _ => "active",
    }
}

fn subject_of(a: &v1::RulesetBypassActor, owner_org: &str) -> Option<String> {
    match v1::RulesetBypassActorKind::try_from(a.kind).ok()? {
        v1::RulesetBypassActorKind::Principal => Some(a.actor_id.clone()),
        v1::RulesetBypassActorKind::Team => Some(if a.actor_id.contains('/') {
            format!("@{}", a.actor_id.trim_start_matches('@'))
        } else {
            format!("@{owner_org}/{}", a.actor_id)
        }),
        _ => None,
    }
}

/// The `approvals` entries of one required-approvals rule (SPEC §9.4). `human_count` humans,
/// when asked for, are one entry that only humans satisfy; the rest of `count` is a second entry
/// in which agents count when `agents_may_approve`.
fn approval_entries(a: &v1::RulesetRuleRequiredApprovals) -> Vec<toml::Table> {
    let mut base = toml::Table::new();
    base.insert(
        "classes".into(),
        toml::Value::Array(a.diff_classes.iter().map(|c| c.clone().into()).collect()),
    );
    base.insert(
        "paths".into(),
        toml::Value::Array(
            a.path_prefixes
                .iter()
                .map(|p| {
                    let s = String::from_utf8_lossy(p).into_owned();
                    format!("/{}", s.trim_start_matches('/')).into()
                })
                .collect(),
        ),
    );
    let from: toml::Value = if a.require_owner_approval {
        "owners".into()
    } else if !a.required_reviewer_groups.is_empty() {
        toml::Value::Array(
            a.required_reviewer_groups
                .iter()
                .map(|g| {
                    if g.starts_with('@') {
                        g.clone().into()
                    } else {
                        format!("@{g}").into()
                    }
                })
                .collect(),
        )
    } else {
        "any".into()
    };
    base.insert("from".into(), from);
    let entry = |count: u32, include_agents: bool| {
        let mut ap = base.clone();
        ap.insert("count".into(), i64::from(count).into());
        ap.insert("include_agents".into(), include_agents.into());
        ap
    };
    let mut out = Vec::new();
    if a.human_count > 0 {
        out.push(entry(a.human_count, false));
        if a.count > a.human_count {
            out.push(entry(a.count, a.agents_may_approve));
        }
    } else {
        out.push(entry(a.count.max(1), a.agents_may_approve));
    }
    out
}

fn render_one(r: &RulesetRec, default_ref: &str) -> toml::Table {
    let rs = &r.ruleset;
    let mut t = toml::Table::new();
    t.insert("name".into(), format!("api-{}", rs.id).into());
    t.insert("mode".into(), mode_of(rs.enforcement).into());
    let conds = rs.conditions.clone().unwrap_or_default();
    let refs: BTreeSet<String> = if rs.target == v1::RulesetTarget::Push as i32 {
        BTreeSet::from(["*".to_string()])
    } else if conds.ref_name_includes.is_empty() {
        BTreeSet::from([ref_pattern("~DEFAULT_BRANCH", rs.target, default_ref)])
    } else {
        conds
            .ref_name_includes
            .iter()
            .map(|p| ref_pattern(p, rs.target, default_ref))
            .collect()
    };
    t.insert(
        "refs".into(),
        toml::Value::Array(refs.into_iter().map(Into::into).collect()),
    );
    let org = r.owner_name.split('/').next().unwrap_or("").to_string();
    let bypass: BTreeSet<String> = rs
        .bypass_actors
        .iter()
        .filter_map(|a| subject_of(a, &org))
        .collect();
    t.insert(
        "bypass".into(),
        toml::Value::Array(bypass.into_iter().map(Into::into).collect()),
    );
    // An API ruleset adds no implicit human approval (design SF5, SPEC §9.4): agents may land
    // every class, and humans are required only through `RulesetRuleRequiredApprovals`.
    t.insert(
        "agents_may_land_without_human".into(),
        toml::Value::Array(DiffClass::ALL.iter().map(|c| c.as_str().into()).collect()),
    );
    let mut checks: BTreeSet<String> = BTreeSet::new();
    let mut approvals = Vec::new();
    let mut push = toml::Table::new();
    let mut deny_paths: BTreeSet<String> = BTreeSet::new();
    let mut deny_ext: BTreeSet<String> = BTreeSet::new();
    for rule in &rs.rules {
        use v1::ruleset_rule::Rule;
        match &rule.rule {
            Some(Rule::RequiredChecks(c)) => {
                checks.extend(c.checks.iter().map(|c| c.name.clone()));
            }
            Some(Rule::RequiredSignatures(_)) => {
                t.insert("require_signed".into(), true.into());
            }
            Some(Rule::LinearHistory(_)) => {
                t.insert("require_linear".into(), true.into());
            }
            Some(Rule::RequiredApprovals(a)) => {
                approvals.extend(approval_entries(a).into_iter().map(toml::Value::Table));
                if a.dismiss_stale_on_push {
                    t.insert("dismiss_stale_approvals".into(), true.into());
                }
                if a.require_last_push_approval {
                    t.insert("require_last_push_approval".into(), true.into());
                }
            }
            Some(Rule::RestrictedPaths(p)) => {
                deny_paths.extend(p.path_globs.iter().cloned());
            }
            Some(Rule::RestrictedExtensions(e)) => {
                deny_ext.extend(
                    e.extensions
                        .iter()
                        .map(|x| x.trim_start_matches('.').to_ascii_lowercase()),
                );
            }
            Some(Rule::MaxFileSize(m)) => {
                push.insert("max_file_size".into(), (m.max_bytes as i64).into());
            }
            Some(Rule::MaxPathLength(m)) => {
                push.insert("max_path_length".into(), i64::from(m.max_bytes).into());
            }
            // Protection itself refuses these on matched refs (moves only through Land).
            Some(Rule::NoDeletion(_)) | Some(Rule::NoForcePush(_)) | Some(Rule::NoCreation(_)) => {}
            _ => {}
        }
    }
    if !checks.is_empty() {
        t.insert(
            "required_checks".into(),
            toml::Value::Array(checks.into_iter().map(Into::into).collect()),
        );
    }
    if !approvals.is_empty() {
        t.insert("approvals".into(), toml::Value::Array(approvals));
    }
    if !deny_paths.is_empty() {
        push.insert(
            "deny_paths".into(),
            toml::Value::Array(deny_paths.into_iter().map(Into::into).collect()),
        );
    }
    if !deny_ext.is_empty() {
        push.insert(
            "deny_extensions".into(),
            toml::Value::Array(deny_ext.into_iter().map(Into::into).collect()),
        );
    }
    if !push.is_empty() {
        t.insert("push".into(), toml::Value::Table(push));
    }
    t
}

/// The `[[rulesets]]` text of `rulesets` (deterministic: sorted by id).
fn render(rulesets: &[RulesetRec], default_ref: &str) -> String {
    if rulesets.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&RulesetRec> = rulesets.iter().collect();
    sorted.sort_by(|a, b| a.ruleset.id.cmp(&b.ruleset.id));
    let mut doc = toml::Table::new();
    doc.insert(
        "rulesets".into(),
        toml::Value::Array(
            sorted
                .iter()
                .map(|r| toml::Value::Table(render_one(r, default_ref)))
                .collect(),
        ),
    );
    toml::to_string(&doc).unwrap_or_default()
}

/// Refuses what the land policy cannot enforce.
fn check_supported(rs: &v1::Ruleset) -> ApiResult<()> {
    let bad = |what: &str| {
        Err(ApiError::invalid(
            "unsupported_rule",
            format!(
                "{what} has no land-policy equivalent on this server (rulesets are enforced through SPEC §9.4 policy)"
            ),
        ))
    };
    if rs.name.trim().is_empty() || rs.name.len() > 100 {
        return Err(ApiError::invalid(
            "bad_name",
            "a ruleset name is 1 to 100 bytes",
        ));
    }
    if let Some(c) = &rs.conditions
        && !c.ref_name_excludes.is_empty()
    {
        return Err(ApiError::invalid(
            "unsupported_condition",
            "ref_name_excludes cannot be enforced by land policy ref patterns; narrow ref_name_includes instead",
        ));
    }
    for a in &rs.bypass_actors {
        match v1::RulesetBypassActorKind::try_from(a.kind) {
            Ok(v1::RulesetBypassActorKind::Principal) | Ok(v1::RulesetBypassActorKind::Team) => {
                if a.actor_id.is_empty() {
                    return Err(ApiError::invalid(
                        "missing_field",
                        "bypass actor_id is required",
                    ));
                }
            }
            _ => {
                return Err(ApiError::invalid(
                    "unsupported_bypass_actor",
                    "bypass actors are principals or teams on this server",
                ));
            }
        }
    }
    for rule in &rs.rules {
        use v1::ruleset_rule::Rule;
        match &rule.rule {
            None => return Err(ApiError::invalid("missing_field", "a rule needs a type")),
            Some(Rule::RequiredDeployments(_)) => return bad("required_deployments"),
            Some(Rule::RequiredWorkflows(_)) => return bad("required_workflows"),
            Some(Rule::RequiredCodeScanning(_)) => return bad("required_code_scanning"),
            Some(Rule::Pattern(_)) => return bad("pattern"),
            Some(Rule::MergeMethods(_)) => return bad("merge_methods"),
            // The forge, rather than the land-policy evaluator, enforces this rule.
            // Validate the subset that its one-at-a-time queue can honour here.
            Some(Rule::MergeQueue(q)) => check_merge_queue(rs, q)?,
            Some(Rule::RequiredApprovals(a)) => {
                if a.require_conversation_resolution {
                    return bad("require_conversation_resolution");
                }
                for c in &a.diff_classes {
                    if DiffClass::parse(c).is_none() {
                        return Err(ApiError::invalid(
                            "bad_diff_class",
                            format!("unknown diff class {c:?}"),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Refuses merge-queue parameters the one-at-a-time forge queue cannot honour.
fn check_merge_queue(rs: &v1::Ruleset, q: &v1::RulesetRuleMergeQueue) -> ApiResult<()> {
    let refuse = |field: &str, why: &str| {
        Err(ApiError::invalid(
            "unsupported_rule",
            format!("merge_queue.{field}: {why}"),
        ))
    };
    let target = v1::RulesetTarget::try_from(rs.target).unwrap_or(v1::RulesetTarget::Branch);
    if matches!(target, v1::RulesetTarget::Tag | v1::RulesetTarget::Push) {
        return Err(ApiError::invalid(
            "unsupported_rule",
            "merge_queue applies to branch rulesets only",
        ));
    }
    if q.merge_method != v1::RulesetMergeMethod::Unspecified as i32 {
        return refuse(
            "merge_method",
            "the queue uses each entry's enqueued method",
        );
    }
    if q.max_entries_to_build != 0 {
        return refuse(
            "max_entries_to_build",
            "the queue builds and checks one entry at a time",
        );
    }
    if q.min_entries_to_merge > 1 {
        return refuse(
            "min_entries_to_merge",
            "the queue lands entries one at a time",
        );
    }
    if q.min_entries_wait_seconds != 0 {
        return refuse(
            "min_entries_wait_seconds",
            "the queue does not batch entries before checking",
        );
    }
    if q.check_timeout_seconds != 0 {
        return refuse(
            "check_timeout_seconds",
            "the queue awaits required checks without a timeout",
        );
    }
    if q.all_entries_must_pass {
        return refuse(
            "all_entries_must_pass",
            "entries are tested and landed independently",
        );
    }
    Ok(())
}

fn has_merge_queue(rs: &v1::Ruleset) -> bool {
    rs.rules
        .iter()
        .any(|r| matches!(r.rule, Some(v1::ruleset_rule::Rule::MergeQueue(_))))
}

/// Whether an active API ruleset with a merge-queue rule requires queueing on `target_ref`.
pub async fn merge_queue_required(
    state: &AppState,
    repo: &RepoCtx,
    target_ref: &str,
) -> ApiResult<bool> {
    let ref_name = target_ref.trim_start_matches("refs/heads/");
    let policy = state.policy(repo, ref_name).await?;
    for (name, rs) in &policy.doc().rulesets {
        if rs.mode != RulesetMode::Active || !rs.refs.iter().any(|p| p.matches(ref_name)) {
            continue;
        }
        let Some(id) = name.strip_prefix("api-") else {
            continue;
        };
        let Some(rec) = get_json::<RulesetRec>(state.kv.as_ref(), NS_RULESET, id).await? else {
            continue;
        };
        if has_merge_queue(&rec.ruleset) && covers(state, &rec, repo).await {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Stricter queue limits from every active matching API ruleset. A zero max is unspecified;
/// otherwise the smallest maximum wins.
pub async fn merge_queue_params(
    state: &AppState,
    repo: &RepoCtx,
    target_ref: &str,
) -> ApiResult<Option<bee_forge::changes::QueueParams>> {
    let ref_name = target_ref.trim_start_matches("refs/heads/");
    let policy = state.policy(repo, ref_name).await?;
    let mut max_entries: Option<u32> = None;
    for (name, rs) in &policy.doc().rulesets {
        if rs.mode != RulesetMode::Active || !rs.refs.iter().any(|p| p.matches(ref_name)) {
            continue;
        }
        let Some(id) = name.strip_prefix("api-") else {
            continue;
        };
        let Some(rec) = get_json::<RulesetRec>(state.kv.as_ref(), NS_RULESET, id).await? else {
            continue;
        };
        if !covers(state, &rec, repo).await {
            continue;
        }
        for rule in &rec.ruleset.rules {
            let Some(v1::ruleset_rule::Rule::MergeQueue(q)) = &rule.rule else {
                continue;
            };
            if q.max_entries_to_merge != 0 {
                max_entries = Some(
                    max_entries.map_or(q.max_entries_to_merge, |n| n.min(q.max_entries_to_merge)),
                );
            }
        }
    }
    Ok(max_entries.map(|max_entries| bee_forge::changes::QueueParams { max_entries }))
}

// ----- JSON form of the effective policy ----------------------------------------------------

fn subjects(s: &BTreeSet<bee_policy::Subject>) -> Value {
    Value::Array(s.iter().map(|x| Value::String(x.to_string())).collect())
}

fn body_json(b: &bee_policy::RuleBody) -> Value {
    json!({
        "allow": b.allow.as_ref().map(subjects),
        "required_checks": b.required_checks,
        "approvals": b.approvals.iter().map(|a| json!({
            "classes": a.classes.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "paths": a.paths,
            "count": a.count,
            "from": match &a.from {
                bee_policy::ApproverSet::Any => json!("any"),
                bee_policy::ApproverSet::Owners => json!("owners"),
                bee_policy::ApproverSet::Subjects(s) => subjects(s),
            },
            "include_agents": a.include_agents,
        })).collect::<Vec<_>>(),
        "agents_may_land_without_human": b.agents_may_land_without_human.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "require_signed": b.require_signed,
        "require_linear": b.require_linear,
        "dismiss_stale_approvals": b.dismiss_stale_approvals,
        "require_last_push_approval": b.require_last_push_approval,
        "push": push_json(&b.push),
    })
}

fn push_json(p: &bee_policy::PushRules) -> Value {
    json!({
        "deny_paths": p.deny_paths.iter().map(|g| g.as_str().to_string()).collect::<Vec<_>>(),
        "deny_extensions": p.deny_extensions,
        "max_file_size": p.max_file_size,
        "max_path_length": p.max_path_length,
    })
}

/// The normalized effective policy as deterministic JSON.
pub fn policy_json(doc: &PolicyDoc) -> String {
    let v = json!({
        "codeowners": doc.codeowners,
        "groups": doc.groups.iter().map(|(k, g)| (k.clone(), subjects(&g.members))).collect::<BTreeMap<_, _>>(),
        "roles": doc.roles.iter().map(|(k, r)| (k.clone(), json!({
            "base": r.base,
            "actions": r.actions.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
        }))).collect::<BTreeMap<_, _>>(),
        "members": doc.members.iter().map(|(k, m)| (k.clone(), subjects(&m.subjects))).collect::<BTreeMap<_, _>>(),
        "access": doc.access.iter().map(|(k, a)| (format!("/{k}"), json!({
            "read": a.read.as_ref().map(subjects),
            "write": a.write.as_ref().map(subjects),
        }))).collect::<BTreeMap<_, _>>(),
        "refs": doc.refs.iter().map(|(p, r)| (p.to_string(), json!({
            "create": r.create.as_ref().map(subjects),
            "update": r.update.as_ref().map(subjects),
            "delete": r.delete.as_ref().map(subjects),
            "force": r.force.as_ref().map(subjects),
        }))).collect::<BTreeMap<_, _>>(),
        "land": doc.land.iter().map(|(p, r)| (p.to_string(), body_json(&r.body))).collect::<BTreeMap<_, _>>(),
        "owners": doc.owners.iter().map(|(k, o)| (format!("/{k}"), Value::Array(o.owners.iter().map(|x| Value::String(x.to_string())).collect()))).collect::<BTreeMap<_, _>>(),
        "push": push_json(&doc.push),
        "paths": {
            "sensitive": doc.paths.sensitive.iter().map(|g| g.as_str().to_string()).collect::<Vec<_>>(),
            "generated": doc.paths.generated.iter().map(|g| g.as_str().to_string()).collect::<Vec<_>>(),
            "exclusive_lease": doc.paths.exclusive_lease.iter().map(|g| g.as_str().to_string()).collect::<Vec<_>>(),
        },
        "rulesets": doc.rulesets.iter().map(|(k, r)| (k.clone(), json!({
            "mode": r.mode.as_str(),
            "refs": r.refs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "bypass": subjects(&r.bypass),
            "body": body_json(&r.body),
        }))).collect::<BTreeMap<_, _>>(),
    });
    serde_json::to_string(&v).unwrap_or_default()
}

/// One plain-language line per rule.
fn summaries(doc: &PolicyDoc) -> Vec<String> {
    let mut out = Vec::new();
    for (p, a) in &doc.access {
        let w = a
            .write
            .as_ref()
            .map(|s| {
                s.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "inherited".into());
        let r = a
            .read
            .as_ref()
            .map(|s| {
                s.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "inherited".into());
        out.push(format!("/{p}: read by {r}; written by {w}"));
    }
    for (p, l) in &doc.land {
        let mut parts = Vec::new();
        if !l.body.required_checks.is_empty() {
            parts.push(format!(
                "checks {}",
                l.body
                    .required_checks
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for a in &l.body.approvals {
            parts.push(format!("{} approval(s)", a.count));
        }
        if l.body.require_signed {
            parts.push("signed changesets".into());
        }
        if l.body.require_linear {
            parts.push("linear history".into());
        }
        out.push(format!(
            "land on {p}: {}",
            if parts.is_empty() {
                "no extra requirements".to_string()
            } else {
                parts.join(", ")
            }
        ));
    }
    for (n, r) in &doc.rulesets {
        out.push(format!(
            "ruleset {n} ({}) on {}",
            r.mode.as_str(),
            r.refs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for (p, o) in &doc.owners {
        out.push(format!(
            "/{p} owned by {}",
            o.owners
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

// ----- the service --------------------------------------------------------------------------

/// The policy service.
pub struct PolicyApi {
    state: Arc<AppState>,
}

impl PolicyApi {
    /// The service over `state`.
    pub fn new(state: Arc<AppState>) -> PolicyApi {
        PolicyApi { state }
    }
}

/// Builds the actor `repo`'s policy sees for a principal of the server's table: its groups
/// plus `@repo-role/<role>` for its own forge role on the repo (SPEC §9.4).
async fn actor_of(state: &AppState, repo: &RepoCtx, id: &str) -> ApiResult<Actor> {
    match state.principal(id).await? {
        Some(r) => {
            let groups = state.groups_of(&r.id, r.groups.clone()).await;
            let actor = r.to_auth().to_ref().to_actor(groups, Vec::new());
            let record = state.repo_record(repo).await?;
            state.actor_on_repo(record.as_deref(), actor).await
        }
        None => Err(ApiError::not_found(format!("principal {id}"))),
    }
}

fn policy_error(e: &bee_policy::PolicyError) -> v1::PolicyValidationIssue {
    v1::PolicyValidationIssue {
        severity: v1::PolicyIssueSeverity::Error as i32,
        file: match e.location.origin {
            bee_policy::Origin::Repo => POLICY_PATH.to_string(),
            bee_policy::Origin::Codeowners => "CODEOWNERS".into(),
            bee_policy::Origin::Org => "org policy".into(),
            bee_policy::Origin::Effective => "effective policy".into(),
        },
        line: e.location.line.unwrap_or(0),
        column: e.location.column.unwrap_or(0),
        key_path: e.location.key.clone(),
        subcode: e.kind.as_str().to_string(),
        message: e.message.clone(),
    }
}

fn epoch_text(state: &AppState, policy: &Policy) -> String {
    use bee_auth::RevocationList;
    bee_policy::repo_epoch(policy.epoch(), state.revocations.counter()).to_string()
}

impl PolicyApi {
    async fn repo_read(&self, caller: &Caller, r: Option<&v1::RepoRef>) -> ApiResult<RepoCtx> {
        let repo = self.state.repo(r).await?;
        self.state.gate(caller, &repo, Action::Read, None).await?;
        Ok(repo)
    }

    async fn effective(
        &self,
        caller: &Caller,
        r: v1::GetEffectivePolicyRequest,
    ) -> ApiResult<v1::PolicyDocument> {
        let s = &self.state;
        let repo = self.repo_read(caller, r.repo.as_ref()).await?;
        let ref_name = if r.r#ref.is_empty() {
            default_ref(s, &repo).await
        } else {
            r.r#ref.clone()
        };
        let history = s.history(&repo);
        let tip = match &r.at {
            Some(at) if !at.expr.is_empty() => {
                Some(crate::services::query::resolve_rev(&history, Some(at)).await?)
            }
            _ => {
                let name = bee_core::RefName::new(ref_name.as_str()).map_err(ApiError::from)?;
                s.meta
                    .get_ref(repo.repo.id, &name)
                    .await?
                    .and_then(|x| x.target)
            }
        };
        let org_text = effective_org_text(s, &repo).await?;
        let fallback = crate::policy::evaluate_at(&history, &org_text, tip.as_ref())
            .await
            .is_err();
        let policy = s.policy_at(&repo, &ref_name, tip.as_ref()).await?;
        let mut sources = vec![v1::PolicySource {
            kind: if s.cfg.policy.org_default_file.is_empty() {
                v1::PolicySourceKind::Builtin
            } else {
                v1::PolicySourceKind::OrgDefault
            } as i32,
            id: repo.org.id.0.to_text(),
            ..Default::default()
        }];
        if let Some(t) = &tip
            && let Some(_) = read_text(&history, t, POLICY_PATH).await?
        {
            let blob = history
                .entry_at(
                    history.changeset(t).await?.root(),
                    &RepoPath::new(POLICY_PATH).map_err(ApiError::from)?,
                )
                .await?
                .map(|e| id_to(&e.id));
            sources.push(v1::PolicySource {
                kind: v1::PolicySourceKind::RepoFile as i32,
                path: POLICY_PATH.into(),
                blob,
                changeset: Some(id_to(t)),
                ..Default::default()
            });
        }
        for name in policy.doc().rulesets.keys() {
            if let Some(id) = name.strip_prefix("api-") {
                sources.push(v1::PolicySource {
                    kind: v1::PolicySourceKind::Ruleset as i32,
                    id: id.to_string(),
                    ..Default::default()
                });
            }
        }
        use bee_auth::RevocationList;
        Ok(v1::PolicyDocument {
            repo: Some(v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::FullName(repo.full_name.clone())),
            }),
            r#ref: ref_name,
            epoch: epoch_text(s, &policy),
            normalized_json: policy_json(policy.doc()),
            sources,
            last_good_fallback: fallback,
            revocation_counter: s.revocations.counter(),
        })
    }

    async fn validate(
        &self,
        caller: &Caller,
        r: v1::ValidatePolicyRequest,
    ) -> ApiResult<v1::ValidatePolicyResponse> {
        let s = &self.state;
        let repo = self.repo_read(caller, r.repo.as_ref()).await?;
        let history = s.history(&repo);
        let candidate = match &r.document {
            Some(v1::validate_policy_request::Document::Toml(t)) => Some(t.clone()),
            Some(v1::validate_policy_request::Document::Revision(rev)) => {
                let cs = crate::services::query::resolve_rev(&history, Some(rev)).await?;
                read_text(&history, &cs, POLICY_PATH).await?
            }
            None => {
                return Err(ApiError::invalid(
                    "missing_field",
                    "toml or revision is required",
                ));
            }
        };
        let org_text = effective_org_text(s, &repo).await?;
        let proposed = match Policy::from_sources(Some(&org_text), candidate.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                return Ok(v1::ValidatePolicyResponse {
                    valid: false,
                    issues: vec![policy_error(&e)],
                    ..Default::default()
                });
            }
        };
        let mut issues = Vec::new();
        if !proposed.doc().access.contains_key("") {
            issues.push(v1::PolicyValidationIssue {
                severity: v1::PolicyIssueSeverity::Warning as i32,
                file: POLICY_PATH.into(),
                subcode: "missing_root_rule".into(),
                message: "no `/` access rule".into(),
                ..Default::default()
            });
        }
        let ref_name = if r.r#ref.is_empty() {
            default_ref(s, &repo).await
        } else {
            r.r#ref.clone()
        };
        let mut replay = Vec::new();
        if r.replay_sample > 0
            && let Some(w) = s.hub.get()
        {
            let current = s.policy(&repo, &ref_name).await?;
            let cx = bee_forge::Cx::system();
            if let Ok(frepo) = w.forge.orgs.repo_any(&cx, &repo.id_text()).await {
                let f = bee_forge::changes::query::ListFilter::default();
                let (crs, _) = w
                    .changes
                    .list(&cx, &frepo, &f, "", r.replay_sample.min(100) as usize)
                    .await
                    .map_err(forge_to_api)?;
                for cr in crs {
                    let Some(head) = cr
                        .revisions
                        .last()
                        .and_then(|rv| rv.head.as_ref())
                        .and_then(|h| ObjectId::from_bytes(&h.raw).ok())
                    else {
                        continue;
                    };
                    let author = match &cr.author {
                        Some(a) => actor_of(s, &repo, &a.id).await.ok(),
                        None => None,
                    };
                    let Some(author) = author else { continue };
                    let ctx = self
                        .land_context(&repo, &cr.target_ref, &head, None, &cr.id, vec![author])
                        .await?;
                    let cur = current.evaluate_land(&ctx);
                    let prop = proposed.evaluate_land(&ctx);
                    replay.push(v1::PolicyReplayDecision {
                        change_request_id: cr.id.clone(),
                        title: cr.title.clone(),
                        diff_class: cr
                            .revisions
                            .last()
                            .map(|x| x.diff_class.clone())
                            .unwrap_or_default(),
                        current_allowed: cur.allowed,
                        proposed_allowed: prop.allowed,
                        missing: prop.missing_ids().iter().map(|m| m.to_string()).collect(),
                    });
                }
            }
        }
        Ok(v1::ValidatePolicyResponse {
            valid: true,
            issues,
            normalized_json: policy_json(proposed.doc()),
            resulting_epoch: epoch_text(s, &proposed),
            rule_summaries: summaries(proposed.doc()),
            replay,
        })
    }

    /// A land context for `head` on `ref_name`: changed paths of the stack (first parents
    /// back to `base`, or the ref's current tip, at most 50), their classes, and the forge's
    /// reviews and checks.
    async fn land_context(
        &self,
        repo: &RepoCtx,
        ref_name: &str,
        head: &ObjectId,
        base: Option<ObjectId>,
        cr_id: &str,
        chain: Vec<Actor>,
    ) -> ApiResult<LandContext> {
        let s = &self.state;
        let history = s.history(repo);
        let derived = s.derived(repo);
        let tip = match base {
            Some(b) => Some(b),
            None => match bee_core::RefName::new(ref_name) {
                Ok(n) => s
                    .meta
                    .get_ref(repo.repo.id, &n)
                    .await?
                    .and_then(|r| r.target),
                Err(_) => None,
            },
        };
        let mut changes = Vec::new();
        let mut commits = Vec::new();
        let mut at = *head;
        let mut has_merges = false;
        for _ in 0..50 {
            if Some(at) == tip {
                break;
            }
            let cs = history.changeset(&at).await?;
            has_merges |= cs.parents().len() > 1;
            let classes: HashMap<String, DiffClass> = match derived.diff_class(&at).await {
                Ok(v) => v
                    .per_path
                    .iter()
                    .filter_map(|(p, c, _)| Some((p.to_string(), DiffClass::parse(c.as_str())?)))
                    .collect(),
                Err(_) => HashMap::new(),
            };
            for e in cs.delta() {
                let p = e.path.to_string();
                changes.push(ChangedPath {
                    class: classes.get(&p).copied().unwrap_or(DiffClass::Logic),
                    size: match e.op {
                        bee_object::DeltaOp::Put { size, .. } => Some(size),
                        bee_object::DeltaOp::Delete => None,
                    },
                    path: p,
                });
            }
            commits.push(CommitInfo {
                id: at.to_text(),
                signature: crate::signatures::verify_changeset(s, repo, &at)
                    .await
                    .map(|v| v.state())
                    .unwrap_or(SignatureState::Missing),
            });
            match cs.parents().first() {
                Some(p) => at = *p,
                None => break,
            }
        }
        let inputs = s
            .forge
            .land_inputs(repo.repo.id, ref_name, head, cr_id)
            .await?;
        let code_alerts = s
            .forge
            .code_alerts(repo.repo.id, ref_name, head, cr_id)
            .await?;
        Ok(LandContext {
            chain,
            ref_name: ref_name.to_string(),
            changes,
            head_revision: head.to_text(),
            last_pusher: inputs.last_pusher,
            reviews: inputs.reviews,
            checks: inputs.checks,
            leases: inputs.leases,
            commits,
            has_merges,
            required_workflows: inputs.required_workflows,
            code_alerts,
        })
    }

    async fn evaluate(
        &self,
        caller: &Caller,
        r: v1::EvaluatePolicyActionRequest,
    ) -> ApiResult<v1::EvaluatePolicyActionResponse> {
        let s = &self.state;
        let repo = self.repo_read(caller, r.repo.as_ref()).await?;
        let chain: Vec<Actor> = if r.principal_id.is_empty() || r.principal_id == caller.id() {
            s.policy_actors(caller, &repo).await?.to_vec()
        } else {
            // Evaluating someone else reveals their access: maintainers only.
            s.gate(caller, &repo, Action::ManageSettings, None).await?;
            vec![actor_of(s, &repo, &r.principal_id).await?]
        };
        let ref_name = if r.r#ref.is_empty() {
            default_ref(s, &repo).await
        } else {
            r.r#ref.clone()
        };
        let policy = s.policy(&repo, &ref_name).await?;
        let paths: Vec<String> = r
            .paths
            .iter()
            .map(|p| {
                String::from_utf8_lossy(p)
                    .trim_start_matches('/')
                    .to_string()
            })
            .collect();
        let mut matched = Vec::new();
        let mut missing = Vec::new();
        let mut eval_results = Vec::new();
        let mut bypass = false;
        let action = r.action.as_str();
        let allowed = match action {
            "land" => {
                let mut ctx = match &r.changeset {
                    Some(c) if !c.raw.is_empty() => {
                        let head = id_from(Some(c), "changeset")?;
                        self.land_context(&repo, &ref_name, &head, None, "", chain.clone())
                            .await?
                    }
                    _ => {
                        let class = if r.diff_class.is_empty() {
                            DiffClass::Logic
                        } else {
                            DiffClass::parse(&r.diff_class).ok_or_else(|| {
                                ApiError::invalid("bad_diff_class", "unknown diff class")
                            })?
                        };
                        LandContext {
                            chain: chain.clone(),
                            ref_name: ref_name.clone(),
                            changes: paths
                                .iter()
                                .map(|p| ChangedPath {
                                    path: p.clone(),
                                    class,
                                    size: Some(0),
                                })
                                .collect(),
                            head_revision: "hypothetical".into(),
                            last_pusher: None,
                            reviews: Vec::new(),
                            checks: Vec::new(),
                            leases: Vec::new(),
                            commits: Vec::new(),
                            has_merges: false,
                            required_workflows: Vec::new(),
                            code_alerts: Vec::new(),
                        }
                    }
                };
                for (i, a) in r.approver_ids.iter().enumerate() {
                    let kind = match s.principal(a).await? {
                        Some(p) => p.policy_kind(),
                        None => bee_policy::PrincipalKind::Human,
                    };
                    let stored = s.principal(a).await?.map(|p| p.groups).unwrap_or_default();
                    let groups = s.groups_of(a, stored).await;
                    ctx.reviews.push(Review {
                        reviewer: bee_auth::PrincipalRef::new(a.as_str(), kind)
                            .to_actor(groups, Vec::new()),
                        verdict: Verdict::Approve,
                        revision: ctx.head_revision.clone(),
                    });
                    let _ = i;
                }
                for c in &r.passed_checks {
                    ctx.checks.push(CheckResult {
                        name: c.clone(),
                        status: CheckStatus::Success,
                    });
                }
                let d = policy.evaluate_land(&ctx);
                decision_matches(&d, &mut matched, &mut eval_results);
                missing = d.missing_ids().iter().map(|m| m.to_string()).collect();
                bypass = !d.allowed
                    && d.missing.iter().all(|m| m.rule.starts_with("ruleset["))
                    && d.log
                        .iter()
                        .filter(|l| l.enforced && l.would_block())
                        .all(|l| {
                            policy.doc().rulesets.get(&l.ruleset).is_some_and(|rs| {
                                chain.iter().all(|a| policy.is_member(a, &rs.bypass))
                            })
                        });
                d.allowed
            }
            "push" | "force_push" | "create_ref" | "delete_ref" => {
                let ra = match action {
                    "push" => RefAction::Update,
                    "force_push" => RefAction::ForceUpdate,
                    "create_ref" => RefAction::Create,
                    _ => RefAction::Delete,
                };
                let d = policy.check_ref_chain(&chain, ra, &ref_name);
                matched.push(v1::PolicyRuleMatch {
                    rule: d.rule.clone(),
                    source: v1::PolicySourceKind::RepoFile as i32,
                    effect: if d.allowed {
                        v1::PolicyEffect::Allow
                    } else {
                        v1::PolicyEffect::Deny
                    } as i32,
                    satisfied: d.allowed,
                    detail: d.detail.clone(),
                    ..Default::default()
                });
                let mut ok = d.allowed;
                if ok && action == "push" && !paths.is_empty() {
                    for p in &paths {
                        let a = policy.check_access_chain(&chain, Action::Commit, p);
                        if !a.allowed {
                            ok = false;
                            missing.push(format!("access:commit:{p}"));
                        }
                    }
                }
                if !d.allowed {
                    missing.push(format!("ref:{}:{ref_name}", action));
                }
                ok
            }
            other => {
                let act = match other {
                    "admin" => Action::ManagePolicy,
                    o => Action::parse(o).ok_or_else(|| {
                        ApiError::invalid("bad_action", format!("unknown action {o:?}"))
                    })?,
                };
                let targets = if paths.is_empty() {
                    vec![String::new()]
                } else {
                    paths.clone()
                };
                let mut ok = true;
                for p in &targets {
                    let d = policy.check_access_chain(&chain, act, p);
                    matched.push(v1::PolicyRuleMatch {
                        rule: d.rule.clone(),
                        source: v1::PolicySourceKind::RepoFile as i32,
                        effect: if d.allowed {
                            v1::PolicyEffect::Allow
                        } else {
                            v1::PolicyEffect::Deny
                        } as i32,
                        satisfied: d.allowed,
                        detail: d.detail.clone(),
                        ..Default::default()
                    });
                    if !d.allowed {
                        ok = false;
                        missing.push(format!("access:{}:{p}", act.as_str()));
                    }
                }
                ok
            }
        };
        Ok(v1::EvaluatePolicyActionResponse {
            allowed,
            matched_rules: matched,
            missing,
            bypass_available: bypass,
            evaluate_mode_results: eval_results,
            epoch: epoch_text(s, &policy),
        })
    }
}

/// Matched rules and evaluate-mode results of a land decision.
fn decision_matches(
    d: &LandDecision,
    matched: &mut Vec<v1::PolicyRuleMatch>,
    eval_results: &mut Vec<v1::RulesetRuleResult>,
) {
    for (reqs, ok) in [(&d.satisfied, true), (&d.missing, false)] {
        for r in reqs {
            let (source, source_id) = match r.rule.strip_prefix("ruleset[api-") {
                Some(rest) => (
                    v1::PolicySourceKind::Ruleset,
                    rest.trim_end_matches(']').to_string(),
                ),
                None => (v1::PolicySourceKind::RepoFile, String::new()),
            };
            matched.push(v1::PolicyRuleMatch {
                rule: r.rule.clone(),
                source: source as i32,
                source_id,
                effect: v1::PolicyEffect::Require as i32,
                satisfied: ok,
                detail: format!("{}: {}", r.id, r.detail),
            });
        }
    }
    for l in &d.log {
        if l.mode != RulesetMode::Evaluate {
            continue;
        }
        let id = l
            .ruleset
            .strip_prefix("api-")
            .unwrap_or(&l.ruleset)
            .to_string();
        eval_results.push(v1::RulesetRuleResult {
            ruleset_id: id,
            rule_id: String::new(),
            rule_type: "ruleset".into(),
            result: if l.would_block() {
                v1::RulesetEvaluationResult::Fail
            } else {
                v1::RulesetEvaluationResult::Pass
            } as i32,
            message: if l.would_block() {
                "would block".into()
            } else {
                "passes".into()
            },
            missing: l.missing.iter().map(|m| m.id.clone()).collect(),
        });
    }
}

// ----- rulesets -----------------------------------------------------------------------------

impl PolicyApi {
    /// Resolves the owner of a ruleset call and requires `manage_policy` there.
    async fn owner(
        &self,
        caller: &Caller,
        repo: Option<&v1::RepoRef>,
        org: &str,
        enterprise: &str,
        write: bool,
    ) -> ApiResult<(String, String, String)> {
        let s = &self.state;
        if let Some(r) = repo.filter(|r| r.repo.is_some()) {
            let ctx = s.repo(Some(r)).await?;
            let action = if write {
                Action::ManagePolicy
            } else {
                Action::Read
            };
            s.gate(caller, &ctx, action, None).await?;
            return Ok(("repo".into(), ctx.id_text(), ctx.full_name));
        }
        if !org.is_empty() {
            let o = crate::services::support::org_by_name_or_id(s, org).await?;
            self.org_perm(caller, &o.id.0.to_text(), write).await?;
            return Ok(("org".into(), o.id.0.to_text(), o.name));
        }
        if !enterprise.is_empty() {
            let w = s.hub.get().ok_or_else(|| {
                ApiError::new(ErrorReason::Unavailable, "the forge is not running")
            })?;
            let cx = crate::forge::landing::caller_cx(caller);
            let e = w
                .enterprise
                .accounts
                .get(&cx, enterprise)
                .await
                .map_err(forge_to_api)?;
            // Every principal of the chain owns the enterprise; reads also admit an
            // administrator of one of the enterprise's own orgs (never of an unrelated org).
            let mut owner = true;
            for id in caller.chain_ids() {
                if !w
                    .enterprise
                    .accounts
                    .is_owner(&cx, &e.id, &id)
                    .await
                    .map_err(forge_to_api)?
                {
                    owner = false;
                    break;
                }
            }
            if !owner && (write || !self.admins_enterprise_org(caller, &e.id).await?) {
                return Err(ApiError::denied(
                    "enterprise rulesets need an enterprise owner",
                ));
            }
            return Ok(("enterprise".into(), e.slug.clone(), e.slug));
        }
        Err(ApiError::invalid(
            "missing_field",
            "repo, org or enterprise is required",
        ))
    }

    /// Whether the caller's chain administers one of the orgs of the enterprise
    /// `enterprise_id`.
    async fn admins_enterprise_org(&self, caller: &Caller, enterprise_id: &str) -> ApiResult<bool> {
        let s = &self.state;
        let Some(w) = s.hub.get() else {
            return Ok(false);
        };
        let cx = bee_forge::Cx::system();
        let mut after = String::new();
        loop {
            let (orgs, next) = w
                .enterprise
                .accounts
                .list_orgs(&cx, enterprise_id, "", &after, 500)
                .await
                .map_err(forge_to_api)?;
            for o in orgs {
                if crate::services::support::is_org_admin(s, caller, &o.org_id).await? {
                    return Ok(true);
                }
            }
            if next.is_empty() {
                return Ok(false);
            }
            after = next;
        }
    }

    /// Org-level permission: `manage_policy` under the org default (or forge org management).
    async fn org_perm(&self, caller: &Caller, org_id: &str, write: bool) -> ApiResult<()> {
        let s = &self.state;
        let org = crate::services::support::org_by_name_or_id(s, org_id).await?;
        let text = s.org_text(&org).await?;
        let policy = Policy::from_sources(Some(&text), None).map_err(|e| {
            ApiError::new(ErrorReason::Internal, format!("invalid org policy: {e}"))
        })?;
        let need = if write {
            Action::ManagePolicy
        } else {
            Action::Read
        };
        let ok = caller
            .actors
            .iter()
            .all(|a| policy.actions_of(a).contains(&need));
        if ok {
            return Ok(());
        }
        if let Some(w) = s.hub.get() {
            let cx = crate::forge::landing::caller_cx(caller);
            let fc = bee_forge::auth::Caller::from_verified(
                caller.token.clone(),
                caller.token_text.clone(),
                cx,
            );
            let perm = if write {
                OrgPerm::Manage
            } else {
                OrgPerm::ViewMembers
            };
            if w.forge.authz.org(&fc, org_id, perm).await.is_ok() {
                return Ok(());
            }
        }
        Err(ApiError::policy(
            ErrorReason::PolicyDenied,
            "members",
            vec![format!("role:{}", need.as_str())],
            format!("{} lacks `{}` in the org", caller.id(), need.as_str()),
        ))
    }

    async fn validate_rendered(&self, rec: &RulesetRec) -> ApiResult<()> {
        let s = &self.state;
        let repos: Vec<RepoCtx> = match rec.owner_kind.as_str() {
            "repo" => {
                let r = s
                    .repo(Some(&v1::RepoRef {
                        repo: Some(v1::repo_ref::Repo::Id(rec.owner_id.clone())),
                    }))
                    .await?;
                vec![r]
            }
            _ => all_repos(s)
                .await?
                .into_iter()
                .filter(|r| rec.owner_kind != "org" || r.org.id.0.to_text() == rec.owner_id)
                .take(1)
                .collect(),
        };
        for repo in repos {
            let mut text = s.org_text(&repo.org).await?;
            text.push('\n');
            text.push_str(&render(
                std::slice::from_ref(rec),
                &default_ref(s, &repo).await,
            ));
            Policy::from_sources(Some(&text), None).map_err(|e| {
                ApiError::invalid(
                    "invalid_policy",
                    format!("the ruleset does not form a valid policy: {e}"),
                )
            })?;
        }
        Ok(())
    }

    fn invalidate(&self) {
        self.state.rulesets.clear();
        self.state.policies.forget_all();
    }

    async fn create_ruleset(
        &self,
        caller: &Caller,
        r: v1::CreateRulesetRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::Ruleset> {
        let s = &self.state;
        use v1::create_ruleset_request::Owner;
        let (kind, id, name) = match &r.owner {
            Some(Owner::Repo(rr)) => self.owner(caller, Some(rr), "", "", true).await?,
            Some(Owner::Org(o)) => self.owner(caller, None, o, "", true).await?,
            Some(Owner::Enterprise(e)) => self.owner(caller, None, "", e, true).await?,
            None => return Err(ApiError::invalid("missing_field", "owner is required")),
        };
        let now = s.now_ms();
        let mut rules = r.rules.clone();
        for (i, rule) in rules.iter_mut().enumerate() {
            if rule.id.is_empty() {
                rule.id = format!("r{}", i + 1);
            }
        }
        let owner = match kind.as_str() {
            "repo" => v1::ruleset::Owner::Repo(v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::FullName(name.clone())),
            }),
            "org" => v1::ruleset::Owner::Org(name.clone()),
            _ => v1::ruleset::Owner::Enterprise(name.clone()),
        };
        let ruleset = v1::Ruleset {
            id: crate::util::new_ulid(now)?.to_text(),
            name: r.name.trim().to_string(),
            owner: Some(owner),
            target: if r.target == 0 {
                v1::RulesetTarget::Branch as i32
            } else {
                r.target
            },
            enforcement: if r.enforcement == 0 {
                v1::RulesetEnforcement::Active as i32
            } else {
                r.enforcement
            },
            conditions: r.conditions.clone(),
            rules,
            bypass_actors: r.bypass_actors.clone(),
            version: 1,
            created_by: Some(v1::Principal {
                id: caller.id().to_string(),
                ..Default::default()
            }),
            created_at: Some(timestamp_ms(now)),
            updated_at: Some(timestamp_ms(now)),
        };
        check_supported(&ruleset)?;
        let rec = RulesetRec {
            owner_kind: kind,
            owner_id: id,
            owner_name: name,
            ruleset,
        };
        let dup = scan_json::<RulesetRec>(s.kv.as_ref(), NS_RULESET, "")
            .await?
            .into_iter()
            .any(|(_, x)| {
                x.owner_kind == rec.owner_kind
                    && x.owner_id == rec.owner_id
                    && x.ruleset.name.eq_ignore_ascii_case(&rec.ruleset.name)
            });
        if dup {
            return Err(ApiError::new(
                ErrorReason::AlreadyExists,
                format!("a ruleset named {:?} exists there", rec.ruleset.name),
            ));
        }
        self.validate_rendered(&rec).await?;
        put_json(s.kv.as_ref(), NS_RULESET, &rec.ruleset.id, &rec).await?;
        self.invalidate();
        s.audit(
            caller,
            meta,
            None,
            &rec.ruleset.id,
            audit_actions::RULESET_CREATE,
            json!({ "owner": rec.owner_name, "enforcement": mode_of(rec.ruleset.enforcement) }),
        )
        .await?;
        Ok(rec.ruleset)
    }

    async fn ruleset(&self, caller: &Caller, id: &str, write: bool) -> ApiResult<RulesetRec> {
        if id.is_empty() {
            return Err(ApiError::invalid("missing_field", "ruleset_id is required"));
        }
        let rec: RulesetRec = get_json(self.state.kv.as_ref(), NS_RULESET, id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("ruleset {id}")))?;
        let check = match rec.owner_kind.as_str() {
            "repo" => {
                self.owner(
                    caller,
                    Some(&v1::RepoRef {
                        repo: Some(v1::repo_ref::Repo::Id(rec.owner_id.clone())),
                    }),
                    "",
                    "",
                    write,
                )
                .await
            }
            "org" => self.owner(caller, None, &rec.owner_id, "", write).await,
            _ => self.owner(caller, None, "", &rec.owner_id, write).await,
        };
        check.map_err(|e| {
            if write {
                e
            } else {
                ApiError::not_found(format!("ruleset {id}"))
            }
        })?;
        Ok(rec)
    }

    async fn update_ruleset(
        &self,
        caller: &Caller,
        r: v1::UpdateRulesetRequest,
        meta: &RequestMeta,
    ) -> ApiResult<v1::Ruleset> {
        let s = &self.state;
        let mut rec = self.ruleset(caller, &r.ruleset_id, true).await?;
        if r.expected_version != 0 && r.expected_version != rec.ruleset.version {
            return Err(ApiError::new(
                ErrorReason::VersionMismatch,
                format!(
                    "ruleset is at version {}, not {}",
                    rec.ruleset.version, r.expected_version
                ),
            ));
        }
        let rs = &mut rec.ruleset;
        if let Some(n) = &r.name {
            rs.name = n.trim().to_string();
        }
        if let Some(t) = r.target {
            rs.target = t;
        }
        if let Some(e) = r.enforcement {
            rs.enforcement = e;
        }
        if r.conditions.is_some() {
            rs.conditions = r.conditions.clone();
        }
        if r.replace_rules {
            rs.rules = r.rules.clone();
            for (i, rule) in rs.rules.iter_mut().enumerate() {
                if rule.id.is_empty() {
                    rule.id = format!("r{}", i + 1);
                }
            }
        }
        if r.replace_bypass_actors {
            rs.bypass_actors = r.bypass_actors.clone();
        }
        rs.version += 1;
        rs.updated_at = Some(timestamp_ms(s.now_ms()));
        check_supported(rs)?;
        self.validate_rendered(&rec).await?;
        // Compare-and-swap on the stored row.
        let before = s.kv.get(NS_RULESET, rec.ruleset.id.as_bytes()).await?;
        let mut stored: RulesetRec = before
            .as_deref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .ok_or_else(|| ApiError::not_found("ruleset"))?;
        if stored.ruleset.version + 1 != rec.ruleset.version {
            return Err(ApiError::new(
                ErrorReason::VersionMismatch,
                "the ruleset changed meanwhile",
            ));
        }
        stored = rec.clone();
        let ok =
            s.kv.apply(vec![
                crate::kv::KvOp::expect(NS_RULESET, stored.ruleset.id.as_bytes().to_vec(), before),
                crate::kv::KvOp::put(
                    NS_RULESET,
                    stored.ruleset.id.as_bytes().to_vec(),
                    serde_json::to_vec(&stored).unwrap_or_default(),
                ),
            ])
            .await?;
        if !ok {
            return Err(ApiError::new(
                ErrorReason::VersionMismatch,
                "the ruleset changed meanwhile",
            ));
        }
        self.invalidate();
        s.audit(
            caller,
            meta,
            None,
            &stored.ruleset.id,
            audit_actions::RULESET_UPDATE,
            json!({ "version": stored.ruleset.version }),
        )
        .await?;
        Ok(stored.ruleset)
    }

    async fn list_rulesets(
        &self,
        caller: &Caller,
        r: v1::ListRulesetsRequest,
    ) -> ApiResult<v1::ListRulesetsResponse> {
        let s = &self.state;
        use v1::list_rulesets_request::Owner;
        let (kind, id, _) = match &r.owner {
            Some(Owner::Repo(rr)) => self.owner(caller, Some(rr), "", "", false).await?,
            Some(Owner::Org(o)) => self.owner(caller, None, o, "", false).await?,
            Some(Owner::Enterprise(e)) => self.owner(caller, None, "", e, false).await?,
            None => return Err(ApiError::invalid("missing_field", "owner is required")),
        };
        let all = scan_json::<RulesetRec>(s.kv.as_ref(), NS_RULESET, "").await?;
        let repo_ctx = if kind == "repo" {
            Some(
                s.repo(Some(&v1::RepoRef {
                    repo: Some(v1::repo_ref::Repo::Id(id.clone())),
                }))
                .await?,
            )
        } else {
            None
        };
        let mut out = Vec::new();
        for (_, rec) in all {
            let own = rec.owner_kind == kind && rec.owner_id == id;
            let inherited = r.include_inherited
                && match (&repo_ctx, rec.owner_kind.as_str()) {
                    (Some(repo), "org" | "enterprise") => covers(s, &rec, repo).await,
                    (None, "enterprise") if kind == "org" => true,
                    _ => false,
                };
            if (own || inherited) && (r.target == 0 || rec.ruleset.target == r.target) {
                out.push(rec.ruleset);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&out, &token, size)?;
        Ok(v1::ListRulesetsResponse {
            rulesets: page,
            page: next_page(next),
        })
    }

    async fn rules_for_ref(
        &self,
        caller: &Caller,
        r: v1::ListRulesetRulesForRefRequest,
    ) -> ApiResult<v1::ListRulesetRulesForRefResponse> {
        let s = &self.state;
        let repo = self.repo_read(caller, r.repo.as_ref()).await?;
        let ref_name = if r.r#ref.is_empty() {
            default_ref(s, &repo).await
        } else {
            r.r#ref.trim_start_matches("refs/heads/").to_string()
        };
        let policy = s.policy(&repo, &ref_name).await?;
        let api: BTreeMap<String, RulesetRec> =
            scan_json::<RulesetRec>(s.kv.as_ref(), NS_RULESET, "")
                .await?
                .into_iter()
                .map(|(_, r)| (r.ruleset.id.clone(), r))
                .collect();
        let mut out = Vec::new();
        for (name, rs) in &policy.doc().rulesets {
            if rs.mode == RulesetMode::Disabled
                || (rs.mode == RulesetMode::Evaluate && !r.include_evaluate_mode)
                || !rs.refs.iter().any(|p| p.matches(&ref_name))
            {
                continue;
            }
            let enforcement = match rs.mode {
                RulesetMode::Evaluate => v1::RulesetEnforcement::Evaluate,
                _ => v1::RulesetEnforcement::Active,
            } as i32;
            match name.strip_prefix("api-").and_then(|id| api.get(id)) {
                Some(rec) => {
                    for rule in &rec.ruleset.rules {
                        out.push(v1::RulesetActiveRule {
                            ruleset_id: rec.ruleset.id.clone(),
                            ruleset_name: rec.ruleset.name.clone(),
                            source: v1::PolicySourceKind::Ruleset as i32,
                            source_owner: rec.owner_name.clone(),
                            enforcement,
                            rule: Some(rule.clone()),
                        });
                    }
                }
                None => {
                    // A policy-file ruleset: its requirements as typed rules.
                    for rule in file_rules(&rs.body) {
                        out.push(v1::RulesetActiveRule {
                            ruleset_id: String::new(),
                            ruleset_name: name.clone(),
                            source: v1::PolicySourceKind::RepoFile as i32,
                            source_owner: repo.full_name.clone(),
                            enforcement,
                            rule: Some(rule),
                        });
                    }
                }
            }
        }
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&out, &token, size)?;
        Ok(v1::ListRulesetRulesForRefResponse {
            rules: page,
            page: next_page(next),
        })
    }
}

/// Typed rules of a policy-file rule body.
fn file_rules(b: &bee_policy::RuleBody) -> Vec<v1::RulesetRule> {
    use v1::ruleset_rule::Rule;
    let mut out = Vec::new();
    let mut add = |id: &str, rule: Rule| {
        out.push(v1::RulesetRule {
            id: id.to_string(),
            rule: Some(rule),
        })
    };
    if !b.required_checks.is_empty() {
        add(
            "required_checks",
            Rule::RequiredChecks(v1::RulesetRuleRequiredChecks {
                checks: b
                    .required_checks
                    .iter()
                    .map(|c| v1::RulesetRequiredCheck {
                        name: c.clone(),
                        app_id: String::new(),
                    })
                    .collect(),
                ..Default::default()
            }),
        );
    }
    for (i, a) in b.approvals.iter().enumerate() {
        add(
            &format!("approvals[{i}]"),
            Rule::RequiredApprovals(v1::RulesetRuleRequiredApprovals {
                count: a.count,
                diff_classes: a.classes.iter().map(|c| c.as_str().to_string()).collect(),
                path_prefixes: a.paths.iter().map(|p| p.clone().into_bytes()).collect(),
                require_owner_approval: matches!(a.from, bee_policy::ApproverSet::Owners),
                agents_may_approve: a.include_agents,
                dismiss_stale_on_push: b.dismiss_stale_approvals,
                require_last_push_approval: b.require_last_push_approval,
                ..Default::default()
            }),
        );
    }
    if b.require_signed {
        add(
            "require_signed",
            Rule::RequiredSignatures(v1::RulesetRuleRequiredSignatures {}),
        );
    }
    if b.require_linear {
        add(
            "require_linear",
            Rule::LinearHistory(v1::RulesetRuleLinearHistory {}),
        );
    }
    if !b.push.deny_paths.is_empty() {
        add(
            "push.deny_paths",
            Rule::RestrictedPaths(v1::RulesetRuleRestrictedPaths {
                path_globs: b
                    .push
                    .deny_paths
                    .iter()
                    .map(|g| g.as_str().to_string())
                    .collect(),
            }),
        );
    }
    if !b.push.deny_extensions.is_empty() {
        add(
            "push.deny_extensions",
            Rule::RestrictedExtensions(v1::RulesetRuleRestrictedExtensions {
                extensions: b.push.deny_extensions.iter().cloned().collect(),
            }),
        );
    }
    if let Some(m) = b.push.max_file_size {
        add(
            "push.max_file_size",
            Rule::MaxFileSize(v1::RulesetRuleMaxFileSize { max_bytes: m }),
        );
    }
    if let Some(m) = b.push.max_path_length {
        add(
            "push.max_path_length",
            Rule::MaxPathLength(v1::RulesetRuleMaxPathLength {
                max_bytes: m.min(u64::from(u32::MAX)) as u32,
            }),
        );
    }
    out
}

// ----- rule insights ------------------------------------------------------------------------

/// A recorded evaluation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EvalRec {
    /// Org id.
    pub org_id: String,
    /// The evaluation.
    pub evaluation: v1::RulesetEvaluation,
}

/// Records the ruleset decision log of a land (called by the coalescer; never blocks it).
pub fn record_land(
    state: Arc<AppState>,
    repo: RepoCtx,
    ref_name: String,
    actor: String,
    head: ObjectId,
    decision: LandDecision,
) {
    if decision.log.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let now = state.now_ms();
        let Ok(id) = crate::util::new_ulid(now) else {
            return;
        };
        let blocked = !decision.allowed;
        let would_block = decision
            .log
            .iter()
            .any(|l| l.mode == RulesetMode::Evaluate && l.would_block());
        let any_eval = decision.log.iter().any(|l| l.mode == RulesetMode::Evaluate);
        let results: Vec<v1::RulesetRuleResult> = decision
            .log
            .iter()
            .map(|l| v1::RulesetRuleResult {
                ruleset_id: l
                    .ruleset
                    .strip_prefix("api-")
                    .unwrap_or(&l.ruleset)
                    .to_string(),
                rule_id: String::new(),
                rule_type: "ruleset".into(),
                result: match l.outcome {
                    RulesetOutcome::Passed => v1::RulesetEvaluationResult::Pass,
                    RulesetOutcome::Failed => v1::RulesetEvaluationResult::Fail,
                    RulesetOutcome::Bypassed => v1::RulesetEvaluationResult::Bypassed,
                } as i32,
                message: format!("{} ({})", l.ruleset, l.mode.as_str()),
                missing: l.missing.iter().map(|m| m.id.clone()).collect(),
            })
            .collect();
        let overall = if results
            .iter()
            .any(|r| r.result == v1::RulesetEvaluationResult::Fail as i32)
        {
            v1::RulesetEvaluationResult::Fail
        } else if results
            .iter()
            .any(|r| r.result == v1::RulesetEvaluationResult::Bypassed as i32)
        {
            v1::RulesetEvaluationResult::Bypassed
        } else {
            v1::RulesetEvaluationResult::Pass
        };
        let ev = v1::RulesetEvaluation {
            id: id.to_text(),
            repo: Some(v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::FullName(repo.full_name.clone())),
            }),
            r#ref: ref_name,
            action: v1::RulesetEvaluationAction::Land as i32,
            changeset: Some(id_to(&head)),
            actor: Some(v1::Principal {
                id: actor,
                ..Default::default()
            }),
            enforcement: if any_eval {
                v1::RulesetEnforcement::Evaluate
            } else {
                v1::RulesetEnforcement::Active
            } as i32,
            result: overall as i32,
            blocked,
            would_block,
            rule_results: results,
            evaluated_at: Some(timestamp_ms(now)),
            ..Default::default()
        };
        let key = format!("{}/{now:020}/{}", repo.id_text(), ev.id);
        let _ = put_json(
            state.kv.as_ref(),
            NS_EVAL,
            &key,
            &EvalRec {
                org_id: repo.org.id.0.to_text(),
                evaluation: ev,
            },
        )
        .await;
    });
}

impl PolicyApi {
    async fn list_evaluations(
        &self,
        caller: &Caller,
        r: v1::ListRulesetEvaluationsRequest,
    ) -> ApiResult<v1::ListRulesetEvaluationsResponse> {
        let s = &self.state;
        use v1::list_ruleset_evaluations_request::Owner;
        let (prefix, org_filter) = match &r.owner {
            Some(Owner::Repo(rr)) => {
                let repo = s.repo(Some(rr)).await?;
                s.gate(caller, &repo, Action::ManagePolicy, None).await?;
                (format!("{}/", repo.id_text()), None)
            }
            Some(Owner::Org(o)) => {
                let org = crate::services::support::org_by_name_or_id(s, o).await?;
                self.org_perm(caller, &org.id.0.to_text(), true).await?;
                (String::new(), Some(org.id.0.to_text()))
            }
            None => {
                return Err(ApiError::invalid(
                    "missing_field",
                    "repo or org is required",
                ));
            }
        };
        let since = r
            .since_at
            .as_ref()
            .map(|t| crate::util::ts_secs(t) * 1000)
            .unwrap_or(0);
        let until = r
            .until_at
            .as_ref()
            .map(|t| crate::util::ts_secs(t) * 1000)
            .unwrap_or(u64::MAX);
        let mut out: Vec<v1::RulesetEvaluation> =
            scan_json::<EvalRec>(s.kv.as_ref(), NS_EVAL, &prefix)
                .await?
                .into_iter()
                .map(|(_, e)| e)
                .filter(|e| org_filter.as_ref().is_none_or(|o| &e.org_id == o))
                .map(|e| e.evaluation)
                .filter(|e| {
                    let at = e
                        .evaluated_at
                        .as_ref()
                        .map(|t| crate::util::ts_secs(t) * 1000)
                        .unwrap_or(0);
                    at >= since
                        && at < until
                        && (r.r#ref.is_empty() || e.r#ref == r.r#ref)
                        && (r.actor_id.is_empty()
                            || e.actor.as_ref().is_some_and(|a| a.id == r.actor_id))
                        && (r.result == 0 || e.result == r.result)
                        && (r.enforcement == 0 || e.enforcement == r.enforcement)
                        && (r.ruleset_id.is_empty()
                            || e.rule_results.iter().any(|x| x.ruleset_id == r.ruleset_id))
                })
                .collect();
        out.reverse();
        let (token, size) = page_of(&r.page);
        let (page, next) = offset_page(&out, &token, size)?;
        Ok(v1::ListRulesetEvaluationsResponse {
            evaluations: page,
            page: next_page(next),
        })
    }

    async fn simulate(
        &self,
        caller: &Caller,
        r: v1::SimulateRulesetRequest,
    ) -> ApiResult<v1::SimulateRulesetResponse> {
        let s = &self.state;
        let rec = match &r.subject {
            Some(v1::simulate_ruleset_request::Subject::RulesetId(id)) => {
                self.ruleset(caller, id, true).await?
            }
            Some(v1::simulate_ruleset_request::Subject::Draft(d)) => {
                use v1::ruleset::Owner;
                let (kind, id, name) = match &d.owner {
                    Some(Owner::Repo(rr)) => self.owner(caller, Some(rr), "", "", true).await?,
                    Some(Owner::Org(o)) => self.owner(caller, None, o, "", true).await?,
                    Some(Owner::Enterprise(e)) => self.owner(caller, None, "", e, true).await?,
                    None => match &r.repo {
                        Some(rr) => self.owner(caller, Some(rr), "", "", true).await?,
                        None => {
                            return Err(ApiError::invalid(
                                "missing_field",
                                "draft.owner is required",
                            ));
                        }
                    },
                };
                let mut draft = d.clone();
                draft.id = "draft".into();
                check_supported(&draft)?;
                RulesetRec {
                    owner_kind: kind,
                    owner_id: id,
                    owner_name: name,
                    ruleset: draft,
                }
            }
            None => {
                return Err(ApiError::invalid(
                    "missing_field",
                    "ruleset_id or draft is required",
                ));
            }
        };
        let now = s.now_ms();
        let since = r
            .since_at
            .as_ref()
            .map(|t| crate::util::ts_secs(t) * 1000)
            .unwrap_or(now.saturating_sub(7 * bee_store::DAY_MS));
        let until = r
            .until_at
            .as_ref()
            .map(|t| crate::util::ts_secs(t) * 1000)
            .unwrap_or(now);
        let max = if r.max_operations == 0 {
            500
        } else {
            r.max_operations.min(5000)
        } as u64;
        let only = match &r.repo {
            Some(rr) => Some(self.repo_read(caller, Some(rr)).await?),
            None => None,
        };
        let repos = match &only {
            Some(rp) => vec![rp.clone()],
            None => all_repos(s).await?,
        };
        // Evaluated as an evaluate-mode ruleset so it never hides the rest of the decision.
        let mut probe = rec.clone();
        probe.ruleset.enforcement = v1::RulesetEnforcement::Evaluate as i32;
        let mut replayed = 0u64;
        let mut truncated = false;
        let mut blocked = Vec::new();
        'outer: for repo in repos {
            if !covers(s, &rec, &repo).await && rec.owner_kind != "repo" {
                continue;
            }
            if rec.owner_kind == "repo" && rec.owner_id != repo.id_text() {
                continue;
            }
            if s.gate(caller, &repo, Action::Read, None).await.is_err() {
                continue;
            }
            let mut text = s.org_text(&repo.org).await?;
            text.push('\n');
            text.push_str(&render(
                std::slice::from_ref(&probe),
                &default_ref(s, &repo).await,
            ));
            let history = s.history(&repo);
            let name = format!("api-{}", rec.ruleset.id);
            for r in s.meta.list_refs(repo.repo.id, "").await? {
                if r.name.as_str().starts_with("ws/") {
                    continue;
                }
                let mut from = 0u64;
                loop {
                    let entries = s.meta.ref_log(repo.repo.id, &r.name, from, 500).await?;
                    let done = entries.len() < 500;
                    for e in &entries {
                        from = e.version + 1;
                        if e.landed.is_empty() || e.at_ms < since || e.at_ms > until {
                            continue;
                        }
                        let Some(head) = e.new else { continue };
                        if replayed >= max {
                            truncated = true;
                            break 'outer;
                        }
                        replayed += 1;
                        let creator = match history.changeset(&head).await {
                            Ok(cs) => history
                                .meta_object(cs.meta())
                                .await
                                .ok()
                                .flatten()
                                .map(|m| m.creator)
                                .filter(|c| !c.is_empty())
                                .unwrap_or_else(|| e.principal.clone()),
                            Err(_) => continue,
                        };
                        let chain = match actor_of(s, &repo, &creator).await {
                            Ok(a) => vec![a],
                            Err(_) => continue,
                        };
                        // The policy in force before the land (the old tip), plus the probe.
                        let Ok(policy) =
                            crate::policy::evaluate_at(&history, &text, e.old.as_ref()).await
                        else {
                            continue;
                        };
                        let ctx = self
                            .land_context(&repo, r.name.as_str(), &head, e.old, "", chain)
                            .await?;
                        let d = policy.evaluate_land(&ctx);
                        if let Some(l) = d.log.iter().find(|l| l.ruleset == name)
                            && l.would_block()
                        {
                            blocked.push(v1::RulesetEvaluation {
                                repo: Some(v1::RepoRef {
                                    repo: Some(v1::repo_ref::Repo::FullName(
                                        repo.full_name.clone(),
                                    )),
                                }),
                                r#ref: r.name.as_str().to_string(),
                                action: v1::RulesetEvaluationAction::Land as i32,
                                changeset: Some(id_to(&head)),
                                actor: Some(v1::Principal {
                                    id: creator.clone(),
                                    ..Default::default()
                                }),
                                enforcement: rec.ruleset.enforcement,
                                result: v1::RulesetEvaluationResult::Fail as i32,
                                would_block: true,
                                rule_results: vec![v1::RulesetRuleResult {
                                    ruleset_id: rec.ruleset.id.clone(),
                                    rule_type: "ruleset".into(),
                                    result: v1::RulesetEvaluationResult::Fail as i32,
                                    message: "would have been blocked".into(),
                                    missing: l.missing.iter().map(|m| m.id.clone()).collect(),
                                    ..Default::default()
                                }],
                                evaluated_at: Some(timestamp_ms(e.at_ms)),
                                ..Default::default()
                            });
                        }
                    }
                    if done {
                        break;
                    }
                }
            }
        }
        Ok(v1::SimulateRulesetResponse {
            would_block_count: blocked.len() as u64,
            evaluations: blocked,
            operations_replayed: replayed,
            truncated,
        })
    }
}

// ----- custom roles -------------------------------------------------------------------------

impl PolicyApi {
    fn wiring(&self) -> ApiResult<Arc<crate::forge::ForgeWiring>> {
        self.state
            .hub
            .get()
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorReason::Unavailable, "custom roles need the forge"))
    }

    async fn forge_org(
        &self,
        caller: &Caller,
        org: &str,
        perm: OrgPerm,
    ) -> ApiResult<(Arc<crate::forge::ForgeWiring>, v1::Org, bee_forge::Cx)> {
        let w = self.wiring()?;
        if org.is_empty() {
            return Err(ApiError::invalid("missing_field", "org is required"));
        }
        let fc = bee_forge::auth::Caller::from_verified(
            caller.token.clone(),
            caller.token_text.clone(),
            crate::forge::landing::caller_cx(caller),
        );
        let o = w
            .forge
            .orgs
            .get_org(&fc.cx, org)
            .await
            .map_err(forge_to_api)?;
        let cx = w
            .forge
            .authz
            .org(&fc, &o.id, perm)
            .await
            .map_err(forge_to_api)?;
        Ok((w, o, cx))
    }
}

macro_rules! policy_rpc {
    ($self:ident, $req:ident, |$caller:ident, $inner:ident, $meta:ident| $body:expr) => {
        $crate::unary!($self.state, $req, |$caller, $inner, $meta| $body)
    };
}

#[tonic::async_trait]
impl PolicyService for PolicyApi {
    async fn get_effective_policy(
        &self,
        req: tonic::Request<v1::GetEffectivePolicyRequest>,
    ) -> Rpc<v1::PolicyDocument> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .effective(&caller, inner))
    }

    async fn validate_policy(
        &self,
        req: tonic::Request<v1::ValidatePolicyRequest>,
    ) -> Rpc<v1::ValidatePolicyResponse> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .validate(&caller, inner))
    }

    async fn evaluate_policy_action(
        &self,
        req: tonic::Request<v1::EvaluatePolicyActionRequest>,
    ) -> Rpc<v1::EvaluatePolicyActionResponse> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .evaluate(&caller, inner))
    }

    async fn get_policy_epoch(
        &self,
        req: tonic::Request<v1::GetPolicyEpochRequest>,
    ) -> Rpc<v1::GetPolicyEpochResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            let repo = self.repo_read(&caller, inner.repo.as_ref()).await?;
            let policy = s.policy(&repo, "main").await?;
            let history = s.history(&repo);
            // When the policy last changed: the newest changeset that touched the policy file.
            let mut changed = repo.repo.created_at_ms;
            if let Ok(tip) = crate::services::query::resolve_rev(&history, None).await
                && let Ok(path) = RepoPath::new(POLICY_PATH)
                && let Ok(page) = history
                    .path_log(
                        &tip,
                        Some(&path),
                        &bee_history::LogOptions {
                            limit: 1,
                            ..Default::default()
                        },
                    )
                    .await
                && let Some(e) = page.entries.first()
                && let Some(m) = &e.meta
            {
                changed = (m.author_time.seconds.max(0) as u64) * 1000;
            }
            use bee_auth::RevocationList;
            Ok::<_, ApiError>(v1::GetPolicyEpochResponse {
                epoch: epoch_text(s, &policy),
                revocation_counter: s.revocations.counter(),
                changed_at: Some(timestamp_ms(changed)),
            })
        })
    }

    async fn create_ruleset(
        &self,
        req: tonic::Request<v1::CreateRulesetRequest>,
    ) -> Rpc<v1::Ruleset> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .create_ruleset(&caller, inner, &meta))
    }

    async fn get_ruleset(&self, req: tonic::Request<v1::GetRulesetRequest>) -> Rpc<v1::Ruleset> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            Ok::<_, ApiError>(
                self.ruleset(&caller, &inner.ruleset_id, false)
                    .await?
                    .ruleset,
            )
        })
    }

    async fn update_ruleset(
        &self,
        req: tonic::Request<v1::UpdateRulesetRequest>,
    ) -> Rpc<v1::Ruleset> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .update_ruleset(&caller, inner, &meta))
    }

    async fn delete_ruleset(
        &self,
        req: tonic::Request<v1::DeleteRulesetRequest>,
    ) -> Rpc<v1::DeleteRulesetResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let rec = self.ruleset(&caller, &inner.ruleset_id, true).await?;
            self.state
                .kv
                .apply(vec![crate::kv::KvOp::delete(
                    NS_RULESET,
                    rec.ruleset.id.as_bytes().to_vec(),
                )])
                .await?;
            self.invalidate();
            self.state
                .audit(
                    &caller,
                    &meta,
                    None,
                    &rec.ruleset.id,
                    audit_actions::RULESET_DELETE,
                    json!({}),
                )
                .await?;
            Ok::<_, ApiError>(v1::DeleteRulesetResponse {})
        })
    }

    async fn list_rulesets(
        &self,
        req: tonic::Request<v1::ListRulesetsRequest>,
    ) -> Rpc<v1::ListRulesetsResponse> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .list_rulesets(&caller, inner))
    }

    async fn list_ruleset_rules_for_ref(
        &self,
        req: tonic::Request<v1::ListRulesetRulesForRefRequest>,
    ) -> Rpc<v1::ListRulesetRulesForRefResponse> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .rules_for_ref(&caller, inner))
    }

    async fn list_ruleset_evaluations(
        &self,
        req: tonic::Request<v1::ListRulesetEvaluationsRequest>,
    ) -> Rpc<v1::ListRulesetEvaluationsResponse> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .list_evaluations(&caller, inner))
    }

    async fn simulate_ruleset(
        &self,
        req: tonic::Request<v1::SimulateRulesetRequest>,
    ) -> Rpc<v1::SimulateRulesetResponse> {
        policy_rpc!(self, req, |caller, inner, meta| self
            .simulate(&caller, inner))
    }

    async fn create_custom_role(
        &self,
        req: tonic::Request<v1::CreateCustomRoleRequest>,
    ) -> Rpc<v1::CustomRole> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let (w, org, cx) = self.forge_org(&caller, &inner.org, OrgPerm::Manage).await?;
            let kind =
                v1::PolicyRoleKind::try_from(inner.kind).unwrap_or(v1::PolicyRoleKind::Unspecified);
            w.enterprise
                .roles
                .create(
                    &cx,
                    &org,
                    kind,
                    &inner.name,
                    &inner.description,
                    &inner.base_role,
                    inner.permissions.clone(),
                )
                .await
                .map_err(forge_to_api)
        })
    }

    async fn get_custom_role(
        &self,
        req: tonic::Request<v1::GetCustomRoleRequest>,
    ) -> Rpc<v1::CustomRole> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let (w, org, cx) = self
                .forge_org(&caller, &inner.org, OrgPerm::ViewMembers)
                .await?;
            w.enterprise
                .roles
                .get(&cx, &org, &inner.role_id)
                .await
                .map_err(forge_to_api)
        })
    }

    async fn update_custom_role(
        &self,
        req: tonic::Request<v1::UpdateCustomRoleRequest>,
    ) -> Rpc<v1::CustomRole> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let (w, org, cx) = self.forge_org(&caller, &inner.org, OrgPerm::Manage).await?;
            let r = w
                .enterprise
                .roles
                .update(
                    &cx,
                    &org,
                    &inner.role_id,
                    bee_forge::enterprise::roles::RolePatch {
                        name: inner.name.clone(),
                        description: inner.description.clone(),
                        base_role: inner.base_role.clone(),
                        permissions: inner.replace_permissions.then(|| inner.permissions.clone()),
                    },
                )
                .await
                .map_err(forge_to_api)?;
            self.state.policies.forget_all();
            Ok::<_, ApiError>(r)
        })
    }

    async fn delete_custom_role(
        &self,
        req: tonic::Request<v1::DeleteCustomRoleRequest>,
    ) -> Rpc<v1::DeleteCustomRoleResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let (w, org, cx) = self.forge_org(&caller, &inner.org, OrgPerm::Manage).await?;
            w.enterprise
                .roles
                .delete(&cx, &org, &inner.role_id)
                .await
                .map_err(forge_to_api)?;
            self.state.policies.forget_all();
            Ok::<_, ApiError>(v1::DeleteCustomRoleResponse {})
        })
    }

    async fn list_custom_roles(
        &self,
        req: tonic::Request<v1::ListCustomRolesRequest>,
    ) -> Rpc<v1::ListCustomRolesResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let (w, org, cx) = self
                .forge_org(&caller, &inner.org, OrgPerm::ViewMembers)
                .await?;
            let (token, size) = page_of(&inner.page);
            let kind =
                v1::PolicyRoleKind::try_from(inner.kind).unwrap_or(v1::PolicyRoleKind::Unspecified);
            let (roles, next) = w
                .enterprise
                .roles
                .list(&cx, &org, kind, &token, size)
                .await
                .map_err(forge_to_api)?;
            Ok::<_, ApiError>(v1::ListCustomRolesResponse {
                roles,
                page: next_page(next),
            })
        })
    }

    async fn list_builtin_roles(
        &self,
        req: tonic::Request<v1::ListBuiltinRolesRequest>,
    ) -> Rpc<v1::ListBuiltinRolesResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let _ = &caller;
            Ok::<_, ApiError>(v1::ListBuiltinRolesResponse {
                roles: bee_forge::enterprise::roles::builtin_roles()
                    .into_iter()
                    .filter(|r| inner.kind == 0 || r.kind == inner.kind)
                    .collect(),
            })
        })
    }

    async fn list_policy_permissions(
        &self,
        req: tonic::Request<v1::ListPolicyPermissionsRequest>,
    ) -> Rpc<v1::ListPolicyPermissionsResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let _ = &caller;
            Ok::<_, ApiError>(v1::ListPolicyPermissionsResponse {
                permissions: bee_forge::enterprise::roles::permission_catalog()
                    .into_iter()
                    .filter(|p| inner.kind == 0 || p.kind == inner.kind)
                    .collect(),
            })
        })
    }

    async fn assign_custom_org_role(
        &self,
        req: tonic::Request<v1::AssignCustomOrgRoleRequest>,
    ) -> Rpc<v1::PolicyOrgRoleAssignment> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let (w, org, cx) = self.forge_org(&caller, &inner.org, OrgPerm::Manage).await?;
            use v1::assign_custom_org_role_request::Assignee;
            let (p, t) = match &inner.assignee {
                Some(Assignee::PrincipalId(p)) => (Some(p.as_str()), None),
                Some(Assignee::TeamId(t)) => (None, Some(t.as_str())),
                None => return Err(ApiError::invalid("missing_field", "assignee is required")),
            };
            let a = w
                .enterprise
                .roles
                .assign_org_role(&cx, &org, &inner.role, p, t)
                .await
                .map_err(forge_to_api)?;
            self.state.policies.forget_all();
            Ok::<_, ApiError>(a)
        })
    }

    async fn unassign_custom_org_role(
        &self,
        req: tonic::Request<v1::UnassignCustomOrgRoleRequest>,
    ) -> Rpc<v1::UnassignCustomOrgRoleResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let (w, org, cx) = self.forge_org(&caller, &inner.org, OrgPerm::Manage).await?;
            use v1::unassign_custom_org_role_request::Assignee;
            let (p, t) = match &inner.assignee {
                Some(Assignee::PrincipalId(p)) => (Some(p.as_str()), None),
                Some(Assignee::TeamId(t)) => (None, Some(t.as_str())),
                None => return Err(ApiError::invalid("missing_field", "assignee is required")),
            };
            w.enterprise
                .roles
                .unassign_org_role(&cx, &org, &inner.role, p, t)
                .await
                .map_err(forge_to_api)?;
            self.state.policies.forget_all();
            Ok::<_, ApiError>(v1::UnassignCustomOrgRoleResponse {})
        })
    }

    async fn list_custom_org_role_assignments(
        &self,
        req: tonic::Request<v1::ListCustomOrgRoleAssignmentsRequest>,
    ) -> Rpc<v1::ListCustomOrgRoleAssignmentsResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let (w, org, cx) = self
                .forge_org(&caller, &inner.org, OrgPerm::ViewMembers)
                .await?;
            let (token, size) = page_of(&inner.page);
            let (all, next) = w
                .enterprise
                .roles
                .list_org_assignments(&cx, &org, &inner.role, &token, size)
                .await
                .map_err(forge_to_api)?;
            use v1::policy_org_role_assignment::Assignee;
            let assignments = all
                .into_iter()
                .filter(|a| {
                    inner.principal_id.is_empty()
                        || matches!(&a.assignee, Some(Assignee::PrincipalId(p)) if *p == inner.principal_id)
                })
                .collect();
            Ok::<_, ApiError>(v1::ListCustomOrgRoleAssignmentsResponse {
                assignments,
                page: next_page(next),
            })
        })
    }

    async fn get_codeowners(
        &self,
        req: tonic::Request<v1::GetCodeownersRequest>,
    ) -> Rpc<v1::GetCodeownersResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            let repo = self.repo_read(&caller, inner.repo.as_ref()).await?;
            let history = s.history(&repo);
            let cs = crate::services::query::resolve_rev(&history, inner.revision.as_ref()).await?;
            let org_text = effective_org_text(s, &repo).await?;
            let policy = crate::policy::evaluate_at(&history, &org_text, Some(&cs)).await;
            let (policy, errors) = match policy {
                Ok(p) => (p, 0),
                Err(_) => (
                    crate::policy::evaluate_at(&history, &org_text, None).await?,
                    1,
                ),
            };
            let file = policy.doc().codeowners.clone().unwrap_or_default();
            let co_text = if file.is_empty() {
                None
            } else {
                read_text(&history, &cs, file.trim_start_matches('/')).await?
            };
            let co = co_text
                .as_deref()
                .and_then(|t| bee_policy::Codeowners::parse(t).ok());
            let read_policy = s.policy_at(&repo, "main", Some(&cs)).await?;
            let mut paths = Vec::new();
            for p in &inner.paths {
                let text = String::from_utf8_lossy(p)
                    .trim_start_matches('/')
                    .to_string();
                if !s.path_allowed(&caller, repo.repo.id, &read_policy, Action::Read, &text) {
                    continue;
                }
                let owners = policy.owners_of(&text);
                let rule = owners.as_ref().map(|(rule, set)| {
                    let (pattern, file_name, line) = match rule.strip_prefix("CODEOWNERS:") {
                        Some(l) => {
                            let line: u32 = l.parse().unwrap_or(0);
                            let pat = co
                                .as_ref()
                                .and_then(|c| c.entries().iter().find(|e| e.line == line))
                                .map(|e| e.pattern.clone())
                                .unwrap_or_default();
                            (pat, file.clone(), line)
                        }
                        None => (
                            rule.trim_start_matches("owners[")
                                .trim_end_matches(']')
                                .to_string(),
                            POLICY_PATH.to_string(),
                            0,
                        ),
                    };
                    v1::CodeownersRule {
                        pattern,
                        owners: set.iter().map(ToString::to_string).collect(),
                        file: file_name,
                        line,
                        section: String::new(),
                    }
                });
                paths.push(v1::CodeownersPathOwners {
                    path: p.clone(),
                    owners: owners
                        .map(|(_, set)| set.iter().map(ToString::to_string).collect())
                        .unwrap_or_default(),
                    rule,
                });
            }
            Ok::<_, ApiError>(v1::GetCodeownersResponse {
                changeset: Some(id_to(&cs)),
                paths,
                error_count: errors,
            })
        })
    }

    async fn validate_codeowners(
        &self,
        req: tonic::Request<v1::ValidateCodeownersRequest>,
    ) -> Rpc<v1::ValidateCodeownersResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            let repo = self.repo_read(&caller, inner.repo.as_ref()).await?;
            let history = s.history(&repo);
            let (file, content) = match &inner.source {
                Some(v1::validate_codeowners_request::Source::Content(c)) => {
                    ("CODEOWNERS".to_string(), c.clone())
                }
                Some(v1::validate_codeowners_request::Source::Revision(rev)) => {
                    let cs = crate::services::query::resolve_rev(&history, Some(rev)).await?;
                    let policy = s.policy_at(&repo, "main", Some(&cs)).await?;
                    let mut found = None;
                    let named = policy.doc().codeowners.clone();
                    for f in named
                        .iter()
                        .map(|f| f.trim_start_matches('/').to_string())
                        .chain(
                            [
                                "CODEOWNERS",
                                ".github/CODEOWNERS",
                                "docs/CODEOWNERS",
                                ".bee/CODEOWNERS",
                            ]
                            .iter()
                            .map(|x| x.to_string()),
                        )
                    {
                        if let Some(t) = read_text(&history, &cs, &f).await? {
                            found = Some((f, t));
                            break;
                        }
                    }
                    found
                        .ok_or_else(|| ApiError::not_found("no CODEOWNERS file at that revision"))?
                }
                None => {
                    return Err(ApiError::invalid(
                        "missing_field",
                        "revision or content is required",
                    ));
                }
            };
            let errors = match bee_policy::Codeowners::parse(&content) {
                Ok(_) => Vec::new(),
                Err(e) => {
                    let line = e.location.line.unwrap_or(0);
                    vec![v1::CodeownersError {
                        line,
                        column: e.location.column.unwrap_or(0),
                        subcode: match e.kind {
                            bee_policy::PolicyErrorKind::InvalidPattern => "bad_pattern",
                            bee_policy::PolicyErrorKind::InvalidSubject => "invalid_owner_syntax",
                            _ => "bad_pattern",
                        }
                        .into(),
                        message: e.message.clone(),
                        source_line: content
                            .lines()
                            .nth(line.saturating_sub(1) as usize)
                            .unwrap_or("")
                            .to_string(),
                        suggestion: String::new(),
                    }]
                }
            };
            Ok::<_, ApiError>(v1::ValidateCodeownersResponse {
                valid: errors.is_empty(),
                file,
                errors,
            })
        })
    }

    async fn get_required_signatures_status(
        &self,
        req: tonic::Request<v1::GetRequiredSignaturesStatusRequest>,
    ) -> Rpc<v1::GetRequiredSignaturesStatusResponse> {
        policy_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            let repo = self.repo_read(&caller, inner.repo.as_ref()).await?;
            let ref_name = if inner.r#ref.is_empty() {
                default_ref(s, &repo).await
            } else {
                inner.r#ref.trim_start_matches("refs/heads/").to_string()
            };
            let policy = s.policy(&repo, &ref_name).await?;
            let doc = policy.doc();
            let by_file = bee_policy::most_specific(doc.land.iter(), |(p, _)| p, &ref_name)
                .filter(|(p, _)| p.matches(&ref_name))
                .is_some_and(|(_, l)| l.body.require_signed);
            let mut by_rulesets = Vec::new();
            for (name, rs) in &doc.rulesets {
                if rs.mode == RulesetMode::Active
                    && rs.body.require_signed
                    && rs.refs.iter().any(|p| p.matches(&ref_name))
                {
                    by_rulesets.push(name.strip_prefix("api-").unwrap_or(name).to_string());
                }
            }
            let mut checks = Vec::new();
            for c in &inner.changesets {
                let id = id_from(Some(c), "changesets")?;
                let v = crate::signatures::verify_changeset(s, &repo, &id).await?;
                checks.push(v1::PolicySignatureCheck {
                    changeset: Some(id_to(&id)),
                    git_oid: v.git_oid.map(|o| o.to_hex()).unwrap_or_default(),
                    state: v.policy_state() as i32,
                    format: v.format().to_string(),
                    key_id: v.verdict.key_id.clone(),
                    signer_principal_id: v.verdict.signer.clone().unwrap_or_default(),
                });
            }
            Ok::<_, ApiError>(v1::GetRequiredSignaturesStatusResponse {
                required: by_file || !by_rulesets.is_empty(),
                required_by_ruleset_ids: by_rulesets,
                required_by_policy_file: by_file,
                checks,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_conditions_become_policy_patterns() {
        let b = v1::RulesetTarget::Branch as i32;
        let t = v1::RulesetTarget::Tag as i32;
        assert_eq!(ref_pattern("~DEFAULT_BRANCH", b, "main"), "main");
        assert_eq!(ref_pattern("~ALL", b, "main"), "*");
        assert_eq!(ref_pattern("refs/heads/release/*", b, "main"), "release/*");
        assert_eq!(ref_pattern("v*", t, "main"), "tags/v*");
        assert_eq!(ref_pattern("feat/**/x", b, "main"), "feat/*");
        assert!(fnmatch("svc-*", "svc-billing"));
        assert!(!fnmatch("svc-*", "web"));
    }

    #[test]
    fn rendered_rulesets_parse_as_policy() {
        let rs = v1::Ruleset {
            id: "01RS".into(),
            name: "two".into(),
            enforcement: v1::RulesetEnforcement::Evaluate as i32,
            target: v1::RulesetTarget::Branch as i32,
            rules: vec![
                v1::RulesetRule {
                    id: "r1".into(),
                    rule: Some(v1::ruleset_rule::Rule::RequiredApprovals(
                        v1::RulesetRuleRequiredApprovals {
                            count: 2,
                            diff_classes: vec!["logic".into()],
                            ..Default::default()
                        },
                    )),
                },
                v1::RulesetRule {
                    id: "r2".into(),
                    rule: Some(v1::ruleset_rule::Rule::RestrictedExtensions(
                        v1::RulesetRuleRestrictedExtensions {
                            extensions: vec![".exe".into()],
                        },
                    )),
                },
            ],
            bypass_actors: vec![v1::RulesetBypassActor {
                kind: v1::RulesetBypassActorKind::Principal as i32,
                actor_id: "01ADMIN".into(),
                mode: 0,
            }],
            ..Default::default()
        };
        check_supported(&rs).unwrap();
        let rec = RulesetRec {
            owner_kind: "repo".into(),
            owner_id: "x".into(),
            owner_name: "dev/x".into(),
            ruleset: rs,
        };
        let text = format!(
            "{}\n{}",
            crate::policy::builtin_org_default(&["01ADMIN".into()]),
            render(&[rec], "main")
        );
        let p = Policy::from_sources(Some(&text), None).unwrap();
        let r = p.doc().rulesets.get("api-01RS").unwrap();
        assert_eq!(r.mode, RulesetMode::Evaluate);
        assert_eq!(r.body.approvals[0].count, 2);
        assert!(r.body.push.deny_extensions.contains("exe"));
        assert!(!policy_json(p.doc()).is_empty());
        let mut bad = v1::Ruleset {
            name: "x".into(),
            ..Default::default()
        };
        bad.rules.push(v1::RulesetRule {
            id: String::new(),
            rule: Some(v1::ruleset_rule::Rule::MergeQueue(
                v1::RulesetRuleMergeQueue {
                    merge_method: v1::RulesetMergeMethod::Squash as i32,
                    ..Default::default()
                },
            )),
        });
        assert!(check_supported(&bad).is_err());
    }

    /// Renders one ruleset of `rules` and parses it back with the org default, as the server
    /// does for a repo.
    fn parsed(rules: Vec<v1::ruleset_rule::Rule>) -> bee_policy::RuleBody {
        let rs = v1::Ruleset {
            id: "01RS".into(),
            name: "shape".into(),
            enforcement: v1::RulesetEnforcement::Active as i32,
            target: v1::RulesetTarget::Branch as i32,
            rules: rules
                .into_iter()
                .enumerate()
                .map(|(i, r)| v1::RulesetRule {
                    id: format!("r{i}"),
                    rule: Some(r),
                })
                .collect(),
            ..Default::default()
        };
        check_supported(&rs).unwrap();
        let rec = RulesetRec {
            owner_kind: "repo".into(),
            owner_id: "x".into(),
            owner_name: "dev/x".into(),
            ruleset: rs,
        };
        let text = format!(
            "{}\n{}",
            crate::policy::builtin_org_default(&["01ADMIN".into()]),
            render(&[rec], "main")
        );
        let p = Policy::from_sources(Some(&text), None).unwrap();
        p.doc().rulesets.get("api-01RS").unwrap().body.clone()
    }

    fn approvals(a: v1::RulesetRuleRequiredApprovals) -> Vec<(u32, bool)> {
        let body = parsed(vec![v1::ruleset_rule::Rule::RequiredApprovals(a)]);
        assert_eq!(
            body.agents_may_land_without_human.len(),
            DiffClass::ALL.len()
        );
        body.approvals
            .iter()
            .map(|a| (a.count, a.include_agents))
            .collect()
    }

    #[test]
    fn rulesets_add_no_implicit_human_approval() {
        let body = parsed(vec![v1::ruleset_rule::Rule::RestrictedPaths(
            v1::RulesetRuleRestrictedPaths {
                path_globs: vec!["secrets/**".into()],
            },
        )]);
        for c in DiffClass::ALL {
            assert!(body.agents_may_land_without_human.contains(&c), "{c}");
        }
        assert!(body.approvals.is_empty());
    }

    #[test]
    fn approvals_without_humans_are_one_entry() {
        let agents = approvals(v1::RulesetRuleRequiredApprovals {
            count: 1,
            agents_may_approve: true,
            require_last_push_approval: true,
            ..Default::default()
        });
        assert_eq!(agents, vec![(1, true)]);
        let humans = approvals(v1::RulesetRuleRequiredApprovals {
            count: 0,
            ..Default::default()
        });
        assert_eq!(humans, vec![(1, false)]);
    }

    #[test]
    fn human_count_renders_a_humans_only_entry() {
        let only = approvals(v1::RulesetRuleRequiredApprovals {
            count: 1,
            human_count: 1,
            agents_may_approve: true,
            ..Default::default()
        });
        assert_eq!(only, vec![(1, false)]);
        let more = approvals(v1::RulesetRuleRequiredApprovals {
            count: 3,
            human_count: 1,
            agents_may_approve: true,
            diff_classes: vec!["logic".into()],
            path_prefixes: vec![b"src".to_vec()],
            required_reviewer_groups: vec!["dev/core".into()],
            ..Default::default()
        });
        assert_eq!(more, vec![(1, false), (3, true)]);
        let body = parsed(vec![v1::ruleset_rule::Rule::RequiredApprovals(
            v1::RulesetRuleRequiredApprovals {
                count: 3,
                human_count: 1,
                diff_classes: vec!["logic".into()],
                path_prefixes: vec![b"src".to_vec()],
                required_reviewer_groups: vec!["dev/core".into()],
                ..Default::default()
            },
        )]);
        assert_eq!(body.approvals.len(), 2);
        for a in &body.approvals {
            assert!(!a.include_agents);
            assert!(a.classes.contains(&DiffClass::Logic) && a.classes.len() == 1);
            assert_eq!(
                a.paths,
                BTreeSet::from([bee_policy::normalize_prefix("/src").unwrap()])
            );
            assert!(matches!(&a.from, bee_policy::ApproverSet::Subjects(s) if s.len() == 1));
        }
    }
}
