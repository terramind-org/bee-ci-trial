//! Translations between `bee-workflow` values and what CI stores or sends: events, the
//! runner's job plan (`plan_json`), permissions and the token actions they grant, refs,
//! scheduler minutes and status enums. All functions here are pure.

use std::collections::{BTreeMap, BTreeSet};

use bee_policy::Action;
use bee_proto::v1::{CheckStatus, KeyValue, WorkflowConclusion};
use bee_workflow::cron::UtcMinute;
use bee_workflow::event::{ChangedPaths, FilterSubject};
use bee_workflow::expr::{Part, Template};
use bee_workflow::graph::JobResult;
use bee_workflow::model::{Access, PERMISSION_SCOPES, Permissions};
use bee_workflow::plan::{JobInstance, PlannedContainer, PlannedStepKind};
use bee_workflow::{ActionsEvent, RepoInfo};
use serde_json::{Value as Json, json};

use super::model::{RepoInfoRecord, StoredEvent};
use super::util::{from_json, to_json};

/// Stores an event.
pub fn store_event(e: &ActionsEvent) -> StoredEvent {
    let mut s = StoredEvent {
        name: e.name.clone(),
        action: e.action.clone().unwrap_or_default(),
        has_action: e.action.is_some(),
        git_ref: e.git_ref.clone(),
        sha: e.sha.clone(),
        base_ref: e.base_ref.clone(),
        head_ref: e.head_ref.clone(),
        payload_json: e.payload.to_json_compact(),
        actor: e.actor.clone(),
        skip_ci: e.skip_ci,
        by_workflow_token: e.by_workflow_token,
        ..Default::default()
    };
    let paths = |s: &mut StoredEvent, c: &ChangedPaths| match c {
        ChangedPaths::Known(p) => {
            s.paths = p.clone();
            s.paths_known = true;
        }
        ChangedPaths::Unknown => s.paths_known = false,
    };
    match &e.subject {
        FilterSubject::None => s.subject = "none".into(),
        FilterSubject::Branch(b, c) => {
            s.subject = "branch".into();
            s.subject_a = b.clone();
            paths(&mut s, c);
        }
        FilterSubject::Tag(t) => {
            s.subject = "tag".into();
            s.subject_a = t.clone();
        }
        FilterSubject::PullRequest(b, c) => {
            s.subject = "pull_request".into();
            s.subject_a = b.clone();
            paths(&mut s, c);
        }
        FilterSubject::MergeGroup(b) => {
            s.subject = "merge_group".into();
            s.subject_a = b.clone();
        }
        FilterSubject::WorkflowRun(n, b, d) => {
            s.subject = "workflow_run".into();
            s.subject_a = n.clone();
            s.subject_b = b.clone();
            s.depth = *d;
        }
        FilterSubject::Schedule(m) => {
            s.subject = "schedule".into();
            s.minute = vec![
                i64::from(m.year),
                i64::from(m.month),
                i64::from(m.day),
                i64::from(m.hour),
                i64::from(m.minute),
            ];
        }
        FilterSubject::Dispatch(p, inputs) => {
            s.subject = "dispatch".into();
            s.subject_a = p.clone();
            s.inputs = inputs
                .iter()
                .map(|(k, v)| KeyValue {
                    key: k.clone(),
                    value: v.clone(),
                })
                .collect();
        }
    }
    s
}

/// Restores a stored event.
pub fn load_event(s: &StoredEvent) -> ActionsEvent {
    let changed = || {
        if s.paths_known {
            ChangedPaths::Known(s.paths.clone())
        } else {
            ChangedPaths::Unknown
        }
    };
    let m = |i: usize| s.minute.get(i).copied().unwrap_or(0);
    let subject = match s.subject.as_str() {
        "branch" => FilterSubject::Branch(s.subject_a.clone(), changed()),
        "tag" => FilterSubject::Tag(s.subject_a.clone()),
        "pull_request" => FilterSubject::PullRequest(s.subject_a.clone(), changed()),
        "merge_group" => FilterSubject::MergeGroup(s.subject_a.clone()),
        "workflow_run" => {
            FilterSubject::WorkflowRun(s.subject_a.clone(), s.subject_b.clone(), s.depth)
        }
        "schedule" => FilterSubject::Schedule(UtcMinute {
            year: m(0) as i32,
            month: m(1) as u32,
            day: m(2) as u32,
            hour: m(3) as u32,
            minute: m(4) as u32,
        }),
        "dispatch" => FilterSubject::Dispatch(
            s.subject_a.clone(),
            s.inputs
                .iter()
                .map(|kv| (kv.key.clone(), kv.value.clone()))
                .collect(),
        ),
        _ => FilterSubject::None,
    };
    ActionsEvent {
        name: s.name.clone(),
        action: s.has_action.then(|| s.action.clone()),
        git_ref: s.git_ref.clone(),
        sha: s.sha.clone(),
        base_ref: s.base_ref.clone(),
        head_ref: s.head_ref.clone(),
        payload: bee_workflow::value::parse_json(&s.payload_json)
            .unwrap_or(bee_workflow::Value::Null),
        actor: s.actor.clone(),
        subject,
        skip_ci: s.skip_ci,
        by_workflow_token: s.by_workflow_token,
    }
}

/// Stores repo facts.
pub fn store_repo(r: &RepoInfo) -> RepoInfoRecord {
    RepoInfoRecord {
        owner: r.owner.clone(),
        name: r.name.clone(),
        default_branch: r.default_branch.clone(),
        default_branch_sha: r.default_branch_sha.clone(),
        server_url: r.server_url.clone(),
        api_url: r.api_url.clone(),
        graphql_url: r.graphql_url.clone(),
    }
}

/// Restores repo facts.
pub fn load_repo(r: &RepoInfoRecord) -> RepoInfo {
    RepoInfo {
        owner: r.owner.clone(),
        name: r.name.clone(),
        default_branch: r.default_branch.clone(),
        default_branch_sha: r.default_branch_sha.clone(),
        server_url: r.server_url.clone(),
        api_url: r.api_url.clone(),
        graphql_url: r.graphql_url.clone(),
    }
}

/// A template as JSON: an array of literal strings and `{"expr": source}` objects. Literal
/// parts stay data; only `expr` parts are ever evaluated.
pub fn template_json(t: &Template) -> Json {
    Json::Array(
        t.parts
            .iter()
            .map(|p| match p {
                Part::Text(s) => Json::String(s.clone()),
                Part::Expr { source, .. } => json!({ "expr": source }),
            })
            .collect(),
    )
}

/// Restores a template from [`template_json`]; `None` when an expression does not parse.
pub fn template_from_json(v: &Json) -> Option<Template> {
    let mut parts = Vec::new();
    for p in v.as_array()? {
        match p {
            Json::String(s) => parts.push(Part::Text(s.clone())),
            Json::Object(o) => {
                let source = o.get("expr")?.as_str()?.to_string();
                let expr = bee_workflow::expr::parse(&source).ok()?;
                parts.push(Part::Expr { source, expr });
            }
            _ => return None,
        }
    }
    Some(Template { parts })
}

fn pairs(v: &[(String, Template)]) -> Json {
    Json::Array(
        v.iter()
            .map(|(k, t)| json!([k, template_json(t)]))
            .collect(),
    )
}

fn container_json(c: &PlannedContainer) -> Json {
    json!({
        "image": c.image,
        "credentials": pairs(&c.credentials),
        "env": pairs(&c.env),
        "ports": c.ports,
        "volumes": c.volumes,
        "options": c.options,
    })
}

/// Run-level facts the runner needs besides the job instance.
#[derive(Clone, Debug, Default)]
pub struct PlanFacts {
    /// Job record id.
    pub job_id: String,
    /// Job id in the workflow.
    pub job_key: String,
    /// The plan-time `github` context.
    pub github: Json,
    /// `inputs`.
    pub inputs: Json,
    /// `vars`.
    pub vars: Json,
    /// `needs`.
    pub needs: Json,
    /// Debug logging.
    pub debug: bool,
    /// Effective permissions (`scope:access`).
    pub permissions: Vec<String>,
}

/// The resolved job a runner executes (format version 1).
pub fn plan_json(inst: &JobInstance, facts: &PlanFacts) -> String {
    let steps: Vec<Json> = inst
        .steps
        .iter()
        .map(|s| {
            let mut o = json!({
                "id": s.id,
                "name": template_json(&s.name),
                "if": s.condition,
                "env": pairs(&s.env),
                "continue_on_error": template_json(&s.continue_on_error),
                "timeout_minutes": s.timeout_minutes.as_ref().map(template_json),
            });
            match &s.kind {
                PlannedStepKind::Run {
                    script,
                    shell,
                    working_directory,
                } => {
                    o["kind"] = json!("run");
                    o["script"] = template_json(script);
                    o["shell"] = json!({ "name": shell.name, "command": shell.command });
                    o["working_directory"] = working_directory.as_ref().map(template_json).into();
                }
                PlannedStepKind::Uses { action, with } => {
                    o["kind"] = json!("uses");
                    o["action"] = json!(action);
                    o["with"] = pairs(with);
                }
            }
            o
        })
        .collect();
    let v = json!({
        "version": 1,
        "job_id": facts.job_id,
        "job_key": facts.job_key,
        "name": inst.name,
        "github": facts.github,
        "inputs": facts.inputs,
        "vars": facts.vars,
        "needs": facts.needs,
        "matrix": inst.matrix.as_ref().map(|m| to_json(&bee_workflow::Value::from(m.clone()))),
        "strategy": inst.strategy.as_ref().map(|s| json!({
            "job-index": s.job_index,
            "job-total": s.job_total,
            "fail-fast": s.fail_fast,
            "max-parallel": s.max_parallel,
        })),
        "runs_on": { "group": inst.runs_on.group, "labels": inst.runs_on.labels },
        "environment": inst.environment.as_ref().map(|e| json!({
            "name": e.name,
            "url": e.url.as_ref().map(template_json),
        })),
        "timeout_minutes": inst.timeout_minutes,
        "continue_on_error": inst.continue_on_error,
        "container": inst.container.as_ref().map(container_json),
        "services": inst.services.iter().map(|(k, c)| json!([k, container_json(c)])).collect::<Vec<_>>(),
        "env": pairs(&inst.env),
        "outputs": pairs(&inst.outputs),
        "steps": steps,
        "debug": facts.debug,
        "permissions": facts.permissions,
    });
    v.to_string()
}

/// Every permission scope with its effective access. `None` means the repository default:
/// `read` gives `contents` and `packages` read access; `write` gives write-all.
pub fn effective_permissions(
    p: Option<&Permissions>,
    default_write: bool,
) -> Vec<(String, Access)> {
    let all = |a: Access| -> Vec<(String, Access)> {
        PERMISSION_SCOPES
            .iter()
            .map(|s| {
                let a = match (*s, a) {
                    ("id-token", Access::Read) => Access::None,
                    ("models", Access::Write) => Access::Read,
                    _ => a,
                };
                (s.to_string(), a)
            })
            .collect()
    };
    match p {
        Some(Permissions::ReadAll) => all(Access::Read),
        Some(Permissions::WriteAll) => all(Access::Write),
        Some(Permissions::Scopes(list)) => PERMISSION_SCOPES
            .iter()
            .map(|s| {
                let a = list
                    .iter()
                    .find(|(k, _)| k == s)
                    .map(|(_, a)| *a)
                    .unwrap_or(Access::None);
                (s.to_string(), a)
            })
            .collect(),
        None if default_write => all(Access::Write),
        None => PERMISSION_SCOPES
            .iter()
            .map(|s| {
                let a = if matches!(*s, "contents" | "packages") {
                    Access::Read
                } else {
                    Access::None
                };
                (s.to_string(), a)
            })
            .collect(),
    }
}

/// Permissions as `scope:access` strings (scopes with access `none` are left out).
pub fn permission_strings(p: &[(String, Access)]) -> Vec<String> {
    p.iter()
        .filter(|(_, a)| *a != Access::None)
        .map(|(s, a)| format!("{s}:{}", a.name()))
        .collect()
}

/// Whether `perms` (as strings) grants at least `want` on `scope`.
pub fn has_permission(perms: &[String], scope: &str, want: Access) -> bool {
    perms.iter().any(|p| match p.split_once(':') {
        Some((s, a)) if s == scope => match want {
            Access::None => true,
            Access::Read => a == "read" || a == "write",
            Access::Write => a == "write",
        },
        _ => false,
    })
}

/// The policy actions a job token may use under its permissions
/// (`docs/spec/workflows.md` §8): any read → `read`; `contents: write` → `commit`,
/// `manage_refs`, `land`; `pull-requests: write` → `review`, `triage`; `issues: write` →
/// `triage`; `checks`/`statuses`/`actions: write` → `commit` (the action set has no narrower
/// check or run action).
pub fn token_actions(perms: &[String]) -> BTreeSet<Action> {
    let mut out = BTreeSet::new();
    if !perms.is_empty() {
        out.insert(Action::Read);
    }
    let w = |s: &str| has_permission(perms, s, Access::Write);
    if w("contents") {
        out.extend([Action::Commit, Action::ManageRefs, Action::Land]);
    }
    if w("pull-requests") {
        out.extend([Action::Review, Action::Triage]);
    }
    if w("issues") {
        out.insert(Action::Triage);
    }
    if w("checks") || w("statuses") || w("actions") {
        out.insert(Action::Commit);
    }
    out
}

/// A bee ref name as a git ref (`main` → `refs/heads/main`, `tags/v1` → `refs/tags/v1`).
pub fn git_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        return name.to_string();
    }
    match name.strip_prefix("tags/") {
        Some(t) => format!("refs/tags/{t}"),
        None => format!("refs/heads/{name}"),
    }
}

/// A git ref as a bee ref name (inverse of [`git_ref`]); other refs are returned unchanged.
pub fn bee_ref(git: &str) -> String {
    if let Some(b) = git.strip_prefix("refs/heads/") {
        b.to_string()
    } else if let Some(t) = git.strip_prefix("refs/tags/") {
        format!("tags/{t}")
    } else {
        git.to_string()
    }
}

/// The UTC minute containing `ms` (civil-from-days, proleptic Gregorian).
pub fn utc_minute(ms: u64) -> UtcMinute {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    UtcMinute {
        year: (if m <= 2 { y + 1 } else { y }) as i32,
        month: m as u32,
        day: d as u32,
        hour: (rem / 3600) as u32,
        minute: ((rem % 3600) / 60) as u32,
    }
}

/// A leg result as a conclusion.
pub fn conclusion_of(r: &str) -> WorkflowConclusion {
    match r {
        "success" => WorkflowConclusion::Success,
        "failure" => WorkflowConclusion::Failure,
        "cancelled" => WorkflowConclusion::Cancelled,
        "skipped" => WorkflowConclusion::Skipped,
        "timed_out" => WorkflowConclusion::TimedOut,
        "neutral" => WorkflowConclusion::Neutral,
        "startup_failure" => WorkflowConclusion::StartupFailure,
        _ => WorkflowConclusion::Unspecified,
    }
}

/// The graph result of a conclusion (`needs.<id>.result`).
pub fn result_of(c: WorkflowConclusion) -> JobResult {
    match c {
        WorkflowConclusion::Success | WorkflowConclusion::Neutral => JobResult::Success,
        WorkflowConclusion::Cancelled => JobResult::Cancelled,
        WorkflowConclusion::Skipped => JobResult::Skipped,
        _ => JobResult::Failure,
    }
}

/// A conclusion's leg-result string.
pub fn result_str(c: WorkflowConclusion) -> &'static str {
    match c {
        WorkflowConclusion::Success | WorkflowConclusion::Neutral => "success",
        WorkflowConclusion::Cancelled => "cancelled",
        WorkflowConclusion::Skipped => "skipped",
        WorkflowConclusion::TimedOut => "timed_out",
        _ => "failure",
    }
}

/// The check status of a job conclusion.
pub fn check_status(c: WorkflowConclusion) -> CheckStatus {
    match c {
        WorkflowConclusion::Success => CheckStatus::Passed,
        WorkflowConclusion::Failure | WorkflowConclusion::StartupFailure => CheckStatus::Failed,
        WorkflowConclusion::Neutral => CheckStatus::Neutral,
        WorkflowConclusion::Cancelled => CheckStatus::Cancelled,
        WorkflowConclusion::Skipped => CheckStatus::Skipped,
        WorkflowConclusion::TimedOut => CheckStatus::TimedOut,
        WorkflowConclusion::ActionRequired => CheckStatus::ActionRequired,
        WorkflowConclusion::Stale => CheckStatus::Stale,
        WorkflowConclusion::Unspecified => CheckStatus::Queued,
    }
}

/// A JSON object of key/value pairs.
pub fn kv_json(kv: &[KeyValue]) -> Json {
    Json::Object(
        kv.iter()
            .map(|p| (p.key.clone(), Json::String(p.value.clone())))
            .collect(),
    )
}

/// Key/value pairs as a workflow object (string values).
pub fn kv_object(kv: &[KeyValue]) -> bee_workflow::value::Object {
    kv.iter()
        .map(|p| (p.key.clone(), bee_workflow::Value::from(p.value.as_str())))
        .collect()
}

/// A map as sorted key/value pairs.
pub fn to_kv(m: &BTreeMap<String, String>) -> Vec<KeyValue> {
    m.iter()
        .map(|(k, v)| KeyValue {
            key: k.clone(),
            value: v.clone(),
        })
        .collect()
}

/// JSON to a workflow value (re-export for callers holding JSON).
pub fn json_value(v: &Json) -> bee_workflow::Value {
    from_json(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_are_civil_utc() {
        let m = utc_minute(1_760_745_600_000 + 3_723_000); // 2025-10-18 01:02:03
        assert_eq!(
            (m.year, m.month, m.day, m.hour, m.minute),
            (2025, 10, 18, 1, 2)
        );
        let m = utc_minute(951_782_400_000); // 2000-02-29
        assert_eq!((m.year, m.month, m.day), (2000, 2, 29));
    }

    #[test]
    fn refs_map_both_ways() {
        assert_eq!(git_ref("main"), "refs/heads/main");
        assert_eq!(git_ref("tags/v1"), "refs/tags/v1");
        assert_eq!(bee_ref("refs/tags/v1"), "tags/v1");
        assert_eq!(bee_ref(&git_ref("release/1.0")), "release/1.0");
    }

    #[test]
    fn permissions_map_to_token_actions() {
        let p = effective_permissions(None, false);
        let s = permission_strings(&p);
        assert_eq!(s, vec!["contents:read", "packages:read"]);
        assert_eq!(token_actions(&s), BTreeSet::from([Action::Read]));
        let w = permission_strings(&effective_permissions(Some(&Permissions::WriteAll), false));
        assert!(token_actions(&w).contains(&Action::Land));
        assert!(!token_actions(&w).contains(&Action::ManageSettings));
        assert!(has_permission(&w, "id-token", Access::Write));
        let none = permission_strings(&effective_permissions(
            Some(&Permissions::Scopes(Vec::new())),
            true,
        ));
        assert!(token_actions(&none).is_empty());
    }

    #[test]
    fn templates_keep_data_separate() {
        let t = Template {
            parts: vec![
                Part::Text("${{ secrets.X }} literal ".into()),
                Part::Expr {
                    source: "matrix.os".into(),
                    expr: bee_workflow::expr::parse("matrix.os").unwrap(),
                },
            ],
        };
        let back = template_from_json(&template_json(&t)).unwrap();
        assert_eq!(back, t);
    }
}
