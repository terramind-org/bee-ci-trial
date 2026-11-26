//! Compare the person's recorded request with durable forge observations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use bee_auth::VerifiedToken;
use bee_proto::v1::*;

use super::detectors::{self, Attempt, LEASH_ACTIONS, finding};
use super::store::key as store_key;
use crate::agents::model::{RunRec, SessionRec};
use crate::agents::registry::repo_ref_id;
use crate::agents::{Agents, RUNS, SESSIONS, SURFACE, pad};
use crate::audit::{AuditDraft, org_scope};
use crate::changes::Changes;
use crate::changes::model::PathClass;
use crate::changes::store::{hex_of, land_rule};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::{ts, ts_ms};
use crate::security::model::{CodeAlertRec, InstanceRec};
use crate::store::{Cursor, Row, Scan, TableDef};
use crate::swarms;

pub(super) static LAND_ATTEMPTS: TableDef = TableDef {
    name: "forge_safety_land_attempts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

#[derive(Clone, PartialEq, prost::Message)]
struct LandAttempt {
    #[prost(string, tag = "1")]
    audit_id: String,
    #[prost(string, tag = "2")]
    effect: String,
    #[prost(enumeration = "ErrorReason", tag = "3")]
    reason: i32,
    #[prost(uint64, tag = "4")]
    time_ms: u64,
    #[prost(message, optional, tag = "5")]
    denial: Option<PolicyDenial>,
}

fn attempt_scope(repo: &Repo, run_id: &str) -> String {
    format!("{}/{}", repo.id, run_id)
}

pub(crate) fn evidence_event(repo: &str, org: &str, run: &str) -> Event {
    Event {
        kind: "safety.evidence_observed".into(),
        repo_id: repo.into(),
        org_id: org.into(),
        subject: Some(SubjectRef {
            kind: SubjectKind::AgentRun as i32,
            repo: repo_ref_id(repo),
            id: run.into(),
        }),
        ..Default::default()
    }
}

/// Associates a native Bee changeset with its proven run credential. Native clients may
/// finish a command without reporting the changeset through the runner event port.
pub async fn record_changeset(
    forge: &Forge,
    cx: &Cx,
    repo: &Repo,
    token: &VerifiedToken,
    id: &ObjectId,
) -> Result<()> {
    let Some(run_id) = swarms::outcomes::verified_run_token(&forge.db, cx, token, repo).await?
    else {
        return Ok(());
    };
    let mut tx = forge.db.begin(SURFACE, cx).await?;
    let Some(row) = tx.get_for_update(&RUNS, &repo.id, &run_id).await? else {
        return Ok(());
    };
    let mut record: RunRec = row.decode()?;
    let mut run = record.run();
    if run.changesets.contains(id) {
        return Ok(());
    }
    run.changesets.push(id.clone());
    record.run = Some(run);
    crate::agents::runs::push_event(
        &mut tx,
        &mut record,
        AgentRunEvent {
            kind: AgentRunEventKind::ChangesetCreated as i32,
            subject: Some(subject(repo, SubjectKind::Changeset, hex_of(id))),
            ..Default::default()
        },
    )
    .await?;
    crate::agents::runs::save_run(&mut tx, &record, &row).await?;
    tx.emit_event(evidence_event(&repo.id, &repo.org_id, &run_id))
        .await?;
    tx.commit().await
}

/// Stores the outcome of a native or pull request landing request made with a verified
/// run credential. The effect key is namespaced by transport.
pub async fn record_landing_attempt(
    forge: &Forge,
    cx: &Cx,
    repo: &Repo,
    token: &VerifiedToken,
    effect: &str,
    error: Option<&ForgeError>,
) -> Result<()> {
    let Some(run_id) = swarms::outcomes::verified_run_token(&forge.db, cx, token, repo).await?
    else {
        return Ok(());
    };
    let mut tx = forge.db.begin(SURFACE, cx).await?;
    let scope = attempt_scope(repo, &run_id);
    let sequence = tx.add_n1(&LAND_ATTEMPTS, &scope, "counter", 1).await?;
    let effect = effect.to_owned();
    let reason = error.map(|error| error.reason);
    let audit_id = tx
        .audit_id(
            AuditDraft::new("safety.landing_attempt", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::AgentRun, &run_id)
                .after(&serde_json::json!({ "effect": effect }).to_string())
                .result(reason.map_or("ok", bee_proto::errors::reason_name)),
        )
        .await?;
    let row = Row::new(scope, pad(sequence as u64)).msg(&LandAttempt {
        audit_id,
        effect,
        reason: reason.unwrap_or_default() as i32,
        time_ms: tx.now_ms(),
        denial: error.and_then(detectors::refusal),
    });
    tx.insert(&LAND_ATTEMPTS, &row).await?;
    tx.emit_event(evidence_event(&repo.id, &repo.org_id, &run_id))
        .await?;
    tx.commit().await
}

/// Mirror a successful native governing mutation into the durable forge audit outbox.
pub async fn record_native_leash(
    agents: &Agents,
    cx: &Cx,
    action: &str,
    target: &str,
    detail: &serde_json::Value,
) -> Result<()> {
    if cx.run_session.is_empty() || !detectors::leash_audit_action(action) {
        return Ok(());
    }
    let run = match agents.run_rec(cx, &cx.run_session).await {
        Ok(run) => run,
        Err(error) if error.reason == ErrorReason::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if run.token_id.is_empty() || !cx.token_blocks.contains(&run.token_id) {
        return Ok(());
    }
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    tx.audit(
        AuditDraft::new(action, &run.org_id)
            .target_id(target)
            .after(&detail.to_string()),
    )
    .await?;
    tx.commit().await
}

pub(super) struct Snapshot {
    pub(super) record: SafetyRecord,
    pub(super) unfinished: bool,
    pub(super) agent_merge_times: Vec<u64>,
}

fn work_ref(repo: &Repo, kind: SafetyWorkKind, id: impl Into<String>) -> SafetyRef {
    SafetyRef {
        repo: repo_ref_id(&repo.id),
        kind: kind as i32,
        id: id.into(),
    }
}

fn subject(repo: &Repo, kind: SubjectKind, id: impl Into<String>) -> SubjectRef {
    SubjectRef {
        kind: kind as i32,
        repo: repo_ref_id(&repo.id),
        id: id.into(),
    }
}

pub(crate) async fn work_for_run(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    run_id: &str,
) -> Result<SafetyRef> {
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    let work = match swarms::store::swarm_for_run(&mut tx, &repo.id, run_id).await? {
        Some(swarm_id) => work_ref(repo, SafetyWorkKind::Swarm, swarm_id),
        None => work_ref(repo, SafetyWorkKind::Run, run_id),
    };
    Ok(work)
}

pub(super) async fn list_refs(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    after: &str,
    size: usize,
) -> Result<(Vec<SafetyRef>, String)> {
    let (items, next) = swarms::store::list(agents, cx, repo, after, size).await?;
    Ok((
        items
            .into_iter()
            .filter_map(|item| match item.work? {
                repo_work_item::Work::Swarm(s) => Some(work_ref(repo, SafetyWorkKind::Swarm, s.id)),
                repo_work_item::Work::Run(r) => Some(work_ref(repo, SafetyWorkKind::Run, r.id)),
            })
            .collect(),
        next,
    ))
}

fn ask(asked: &mut Vec<SafetyAsk>, text: &str, source: SubjectRef) {
    if !text.is_empty()
        && !asked
            .iter()
            .any(|fact| fact.text == text && fact.source.as_ref() == Some(&source))
    {
        asked.push(SafetyAsk {
            text: text.to_owned(),
            source: Some(source),
        });
    }
}

async fn issue_ask(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    reference: &str,
    asked: &mut Vec<SafetyAsk>,
) -> Result<()> {
    let Ok(number) = reference.trim().trim_start_matches('#').parse() else {
        return Ok(());
    };
    match agents
        .forge
        .work
        .get_issue_by_number(cx, &repo.into(), number)
        .await
    {
        Ok(issue) if issue.repo_id == repo.id => {
            let source = subject(repo, SubjectKind::Issue, issue.number.to_string());
            ask(asked, &issue.title, source.clone());
            ask(asked, &issue.body, source);
        }
        // An issue transfer does not confer access to the destination repository.
        Ok(_) => {}
        Err(error) if error.reason == ErrorReason::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn public_principal(mut principal: Principal) -> Principal {
    principal.external_ids.clear();
    principal
}

pub(super) fn answerable(run: &RunRec) -> Option<Principal> {
    let starter = run.run.as_ref()?.started_by.as_ref();
    starter
        .filter(|actor| actor.kind() == PrincipalKind::Human)
        .cloned()
        .map(public_principal)
        .or_else(|| {
            (!run.owner_id.is_empty()).then(|| Principal {
                id: run.owner_id.clone(),
                kind: PrincipalKind::Human as i32,
                ..Default::default()
            })
        })
}

pub(super) async fn answerable_for_work(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    work: &SafetyRef,
) -> Result<Option<Principal>> {
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    match work.kind() {
        SafetyWorkKind::Run => tx
            .get_msg::<RunRec>(&RUNS, &repo.id, &work.id)
            .await?
            .map(|(record, _)| answerable(&record))
            .ok_or_else(|| ForgeError::not_found("Agent run").with_subcode("safety_work_missing")),
        SafetyWorkKind::Swarm => Ok(swarms::store::get_swarm(&mut tx, &repo.id, &work.id)
            .await?
            .owner
            .map(public_principal)),
        SafetyWorkKind::Unspecified => Err(ForgeError::missing("work.kind")),
    }
}

fn shell_command(event: &AgentRunEvent) -> Option<String> {
    static SCANNER: LazyLock<bee_secrets::Scanner> = LazyLock::new(bee_secrets::Scanner::builtin);
    let payload: serde_json::Value = match event.kind() {
        AgentRunEventKind::ToolResult
            if matches!(
                event.tool_name.as_str(),
                "Bash" | "exec_command" | "command_execution" | "shell_command"
            ) =>
        {
            serde_json::from_str(&event.tool_payload).ok()?
        }
        AgentRunEventKind::Log => {
            let log: serde_json::Value = serde_json::from_str(&event.text).ok()?;
            if log.get("type")?.as_str()? != "item.completed" {
                return None;
            }
            let item = log.get("item")?;
            if item.get("type")?.as_str()? != "command_execution" {
                return None;
            }
            item.clone()
        }
        _ => return None,
    };
    let command = payload
        .get("command")
        .or_else(|| payload.get("cmd"))?
        .as_str()?;
    if command.trim().is_empty() || !SCANNER.is_scannable(command.as_bytes()) {
        return None;
    }
    // Commands are audit evidence. The repository's source-file bypass marker cannot
    // suppress the built-in secret patterns in this context.
    let scanned = command.replace(bee_secrets::INLINE_ALLOW, "");
    if !SCANNER
        .scan("agent-run-command", scanned.as_bytes())
        .is_empty()
    {
        return Some("[Command redacted because it contains a secret pattern]".into());
    }
    Some(command.to_owned())
}

struct Build {
    record: SafetyRecord,
    paths: BTreeSet<String>,
    commands: BTreeSet<String>,
    merged: BTreeSet<String>,
    scan_heads: BTreeMap<Vec<u8>, SubjectRef>,
    merge_candidates: BTreeMap<String, (u32, u64)>,
    time_ms: u64,
}

impl Build {
    fn new(work: SafetyRef) -> Self {
        Self {
            record: SafetyRecord {
                work: Some(work),
                ..Default::default()
            },
            paths: BTreeSet::new(),
            commands: BTreeSet::new(),
            merged: BTreeSet::new(),
            scan_heads: BTreeMap::new(),
            merge_candidates: BTreeMap::new(),
            time_ms: 0,
        }
    }

    fn at(&mut self, time: &Option<pbjson_types::Timestamp>) -> u64 {
        let time_ms = time.as_ref().map_or(0, ts_ms);
        self.time_ms = self.time_ms.max(time_ms);
        time_ms
    }

    fn signal(
        &mut self,
        kind: SafetySignalKind,
        severity: SafetySeverity,
        observation: impl Into<String>,
        subjects: Vec<SubjectRef>,
        paths: Vec<String>,
        time_ms: u64,
    ) {
        self.time_ms = self.time_ms.max(time_ms);
        let mut finding = finding(kind, severity, observation, subjects, paths, time_ms);
        let work = self.record.work.as_ref().expect("Build::new sets the work");
        finding.id = format!(
            "{}-{}",
            finding.id,
            blake3::hash(store_key(work).as_bytes()).to_hex()
        );
        self.record.findings.push(finding);
    }

    fn identify_signal(&mut self, identity: &str) {
        let Some(finding) = self.record.findings.last_mut() else {
            return;
        };
        let suffix = blake3::hash(identity.as_bytes());
        finding.id = format!("{}-{suffix}", finding.id);
    }

    fn finish(mut self, unfinished: bool, agent_merge_times: Vec<u64>) -> Snapshot {
        self.record.findings.sort_by(|a, b| a.id.cmp(&b.id));
        self.record.findings.dedup_by(|a, b| a.id == b.id);
        self.record.verdict = detectors::verdict(&self.record.findings, unfinished) as i32;
        self.record.agents.sort_by(|a, b| a.id.cmp(&b.id));
        self.record.agents.dedup_by(|a, b| a.id == b.id);
        self.record.done = Some(SafetyDone {
            paths: self.paths.into_iter().collect(),
            commands: self.commands.into_iter().collect(),
            // Tool arguments and allowed egress rules do not establish observed destinations.
            network_destinations: Vec::new(),
            merged_change_request_ids: self.merged.into_iter().collect(),
        });
        self.record.updated_at = Some(ts(self.time_ms));
        Snapshot {
            record: self.record,
            unfinished,
            agent_merge_times,
        }
    }
}

async fn attempts(agents: &Agents, cx: &Cx, repo: &Repo, run_id: &str) -> Result<Vec<Attempt>> {
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    let scope = attempt_scope(repo, run_id);
    let mut cursor = None;
    let mut out = Vec::new();
    loop {
        let rows = tx
            .scan(&LAND_ATTEMPTS, &Scan::scope(&scope, 128).after(cursor))
            .await?;
        cursor = rows.last().map(Cursor::after);
        let done = rows.len() < 128;
        for row in rows {
            if row.id == "counter" {
                continue;
            }
            let attempt: LandAttempt = row.decode()?;
            out.push(Attempt {
                reason: attempt.reason(),
                id: attempt.audit_id,
                effect: attempt.effect,
                time_ms: attempt.time_ms,
                denial: attempt.denial,
            });
        }
        if done {
            return Ok(out);
        }
    }
}

async fn audit_page(
    forge: &Forge,
    cx: &Cx,
    repo: &Repo,
    filter: &AuditLogFilter,
) -> Result<Vec<AuditEntry>> {
    let mut out = Vec::new();
    let mut cursor = String::new();
    loop {
        let (entries, next) = forge
            .audit
            .query(cx, &org_scope(&repo.org_id), filter, true, &cursor, 128)
            .await?;
        out.extend(entries);
        if next.is_empty() {
            return Ok(out);
        }
        cursor = next;
    }
}

fn human_approved(reviews: &[Review], revision: u32, merged_at: u64) -> bool {
    reviews.iter().any(|review| {
        review.revision == revision
            && review.verdict() == Verdict::Approve
            && !review.dismissed
            && review
                .created_at
                .as_ref()
                .is_some_and(|at| ts_ms(at) <= merged_at)
            && review
                .reviewer
                .as_ref()
                .is_some_and(|person| person.kind() == PrincipalKind::Human)
    })
}

async fn agent_merges(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    candidates: &BTreeMap<String, (u32, u64)>,
) -> Result<Vec<u64>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let filter = AuditLogFilter {
        action_prefixes: vec!["cr.land".into()],
        repo: repo_ref_id(&repo.id),
        start: candidates
            .values()
            .map(|(_, time)| time)
            .min()
            .map(|time| ts(time.saturating_sub(1))),
        ..Default::default()
    };
    let entries = audit_page(&agents.forge, cx, repo, &filter).await?;
    let mut times = BTreeSet::new();
    for entry in entries {
        if entry.action != "cr.land"
            || entry.result != "ok"
            || !entry
                .actor
                .as_ref()
                .is_some_and(|actor| actor.kind() == PrincipalKind::Agent)
        {
            continue;
        }
        let Some(target) = entry.target else { continue };
        let Some(&(revision, time)) = candidates.get(&target.id) else {
            continue;
        };
        let reviews = agents.changes.raw_reviews(cx, &target.id).await?;
        if !human_approved(&reviews, revision, time) {
            times.insert(time);
        }
    }
    Ok(times.into_iter().collect())
}

struct RunWork {
    rec: RunRec,
    run: AgentRun,
    claims: Vec<Vec<u8>>,
    handed_back: bool,
    finished_at_ms: u64,
}

async fn run_work(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    run_id: &str,
    claims: Option<Vec<Vec<u8>>>,
    handed_back: bool,
) -> Result<RunWork> {
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    let (rec, _) = tx
        .get_msg::<RunRec>(&RUNS, &repo.id, run_id)
        .await?
        .ok_or_else(|| ForgeError::not_found("Agent run").with_subcode("safety_work_missing"))?;
    let run = rec.run();
    let claims = claims.unwrap_or_else(|| {
        run.scope
            .as_ref()
            .map(|s| s.write_paths.clone())
            .unwrap_or_default()
    });
    let finished_at_ms = run.finished_at.as_ref().map_or(0, ts_ms);
    Ok(RunWork {
        rec,
        run,
        claims,
        handed_back,
        finished_at_ms,
    })
}

async fn reported_outputs(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    run: &AgentRun,
) -> Result<(BTreeSet<String>, BTreeMap<Vec<u8>, ObjectId>)> {
    let mut changes: BTreeSet<String> =
        swarms::outcomes::changes(&agents.db, cx, &repo.id, &run.id)
            .await?
            .into_iter()
            .collect();
    let mut changesets: BTreeMap<_, _> = run
        .changesets
        .iter()
        .map(|cs| (cs.raw.clone(), cs.clone()))
        .collect();
    let session = {
        let mut tx = agents.db.begin(SURFACE, cx).await?;
        tx.get_msg::<SessionRec>(&SESSIONS, &repo.id, &run.session)
            .await?
    };
    if let Some((session, _)) = session {
        changes.extend(session.change_request_ids);
        changesets.extend(
            session
                .changesets
                .into_iter()
                .map(|cs| (cs.raw.clone(), cs)),
        );
    }
    for number in &run.change_requests {
        if let Ok(number) = number.parse() {
            match agents.changes.get_by_number(cx, repo, number).await {
                Ok(change) => {
                    changes.insert(change.id);
                }
                Err(error) if error.reason == ErrorReason::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok((changes, changesets))
}

async fn gather_checks(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    target_ref: &str,
    head: &ObjectId,
) -> Result<Vec<String>> {
    let policy = agents.changes.effective_policy(repo).await?;
    let required: BTreeSet<String> = land_rule(&policy, target_ref)
        .into_iter()
        .chain(
            policy
                .doc()
                .rulesets
                .values()
                .filter(|ruleset| {
                    ruleset.mode == bee_policy::RulesetMode::Active
                        && ruleset
                            .refs
                            .iter()
                            .any(|pattern| pattern.matches(target_ref))
                })
                .map(|ruleset| &ruleset.body),
        )
        .flat_map(|rule| rule.required_checks.iter().cloned())
        .collect();
    let hex = hex_of(head);
    let observed = agents
        .forge
        .checks
        .check_results(cx, &repo.id, &hex)
        .await?;
    let suites = agents
        .forge
        .checks
        .list_suites(cx, &repo.id, Some(&hex), "")
        .await?;
    let completed = !suites.is_empty()
        && suites.iter().all(|suite| {
            !matches!(
                suite.status(),
                CheckStatus::Unspecified | CheckStatus::Queued | CheckStatus::Running
            )
        });
    let workflows = if let Some(facts) = agents.changes.land_facts() {
        facts
            .safety_workflows(repo, std::slice::from_ref(head))
            .await?
    } else {
        Vec::new()
    };
    Ok(detectors::missing_checks(
        &required, &observed, &workflows, completed,
    ))
}

struct DiffEvidence<'a> {
    paths: &'a [PathClass],
    base: Option<&'a ObjectId>,
    head: &'a ObjectId,
    origin: SubjectRef,
    time_ms: u64,
}

async fn gather_diff(
    agents: &Agents,
    repo: &Repo,
    run: &RunWork,
    evidence: DiffEvidence<'_>,
    build: &mut Build,
) -> Result<()> {
    let DiffEvidence {
        paths,
        base,
        head,
        origin,
        time_ms,
    } = evidence;
    build
        .paths
        .extend(paths.iter().map(|path| path.path.clone()));
    build.scan_heads.insert(head.raw.clone(), origin.clone());
    let run_subject = subject(repo, SubjectKind::AgentRun, &run.run.id);
    let subjects = if origin == run_subject {
        vec![origin]
    } else {
        vec![run_subject, origin]
    };
    let outside = detectors::outside_paths(paths, &run.claims);
    if !outside.is_empty() {
        build.signal(
            SafetySignalKind::OutsideTask,
            SafetySeverity::Note,
            "Changed paths outside this run’s recorded claim",
            subjects.clone(),
            outside,
            time_ms,
        );
    }
    let (outside_leash, scoped_leash): (Vec<_>, Vec<_>) =
        detectors::leash_paths(paths, &run.rec.instructions_path)
            .into_iter()
            .partition(|path| {
                !run.claims.is_empty() && !detectors::claimed_path(path, &run.claims)
            });
    for (severity, observation, paths) in [
        (
            SafetySeverity::Severe,
            "Changed policy or instructions outside the recorded scope",
            outside_leash,
        ),
        (
            SafetySeverity::Warning,
            "Changed repository policy or agent instructions",
            scoped_leash,
        ),
    ] {
        if !paths.is_empty() {
            build.signal(
                SafetySignalKind::OwnLeash,
                severity,
                observation,
                subjects.clone(),
                paths,
                time_ms,
            );
            build.identify_signal(if severity == SafetySeverity::Severe {
                "outside"
            } else {
                "within-or-unknown"
            });
        }
    }

    let diff = match base {
        Some(base) if !paths.is_empty() => {
            agents
                .changes
                .vcs()
                .diff(repo, base, head, &[], true, 0)
                .await?
        }
        _ => Vec::new(),
    };
    let weakened = detectors::weakened_tests(paths, &diff);
    if !weakened.is_empty() {
        build.signal(
            SafetySignalKind::TestsWeakened,
            SafetySeverity::Warning,
            "Removed tests, added skip markers, or edited a CI workflow",
            subjects,
            weakened,
            time_ms,
        );
    }
    Ok(())
}

async fn gather_run(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    run: &RunWork,
    build: &mut Build,
) -> Result<()> {
    let run_subject = subject(repo, SubjectKind::AgentRun, &run.run.id);
    let creation_ms = build.at(&run.run.created_at);
    build.at(&run.run.started_at);
    build.at(&run.run.finished_at);
    if let Some(task) = &run.run.task {
        issue_ask(agents, cx, repo, &task.issue, &mut build.record.asked).await?;
        ask(&mut build.record.asked, &task.prompt, run_subject.clone());
    }
    if let Some(agent) = run.run.agent.clone() {
        build.record.agents.push(public_principal(agent));
    }
    let event_kinds = [
        AgentRunEventKind::Log as i32,
        AgentRunEventKind::ToolResult as i32,
        AgentRunEventKind::SteeringMessage as i32,
    ];
    let mut after = 0;
    loop {
        let events = agents
            .run_events(cx, &run.run.id, after, &event_kinds, 128)
            .await?;
        let done = events.len() < 128;
        for event in events {
            after = event.seq;
            build.at(&event.time);
            if let Some(command) = shell_command(&event) {
                build.commands.insert(command);
            }
            if event.kind() == AgentRunEventKind::SteeringMessage
                && event
                    .author
                    .as_ref()
                    .is_some_and(|person| person.kind() == PrincipalKind::Human)
            {
                ask(&mut build.record.asked, &event.text, run_subject.clone());
            }
        }
        if done {
            break;
        }
    }
    let land_attempts = attempts(agents, cx, repo, &run.run.id).await?;
    for (first, second, requirement) in detectors::repeated_after_refusal(&land_attempts) {
        build.signal(
            SafetySignalKind::AroundRefusal,
            SafetySeverity::Warning,
            format!(
                "Attempted {} again while {requirement} was unmet (audit {} and {})",
                detectors::native_effect(&first.effect),
                first.id,
                second.id
            ),
            vec![run_subject.clone()],
            Vec::new(),
            second.time_ms,
        );
        build.identify_signal(&format!(
            "{} {requirement}",
            detectors::native_effect(&first.effect)
        ));
    }
    if !run.rec.token_id.is_empty() {
        let filter = AuditLogFilter {
            action_prefixes: LEASH_ACTIONS
                .iter()
                .map(|action| (*action).into())
                .collect(),
            start: run.run.created_at,
            ..Default::default()
        };
        for entry in audit_page(&agents.forge, cx, repo, &filter).await? {
            if detectors::leash_action(&entry, &run.rec.token_id) {
                let mut subjects = vec![run_subject.clone()];
                if entry
                    .repo
                    .as_ref()
                    .is_some_and(|reference| Some(reference) != repo_ref_id(&repo.id).as_ref())
                {
                    continue;
                }
                if let Some(target) = entry.target {
                    subjects.push(target);
                }
                build.signal(
                    SafetySignalKind::OwnLeash,
                    SafetySeverity::Severe,
                    format!(
                        "Changed governing settings through {} (audit {})",
                        entry.action, entry.id
                    ),
                    subjects,
                    Vec::new(),
                    entry.time.as_ref().map_or(creation_ms, ts_ms),
                );
                build.identify_signal(&entry.action);
            }
        }
    }

    let (changes, mut changesets) = reported_outputs(agents, cx, repo, &run.run).await?;
    let mut check_targets: BTreeMap<Vec<u8>, (ObjectId, String, SubjectRef)> = BTreeMap::new();
    for id in changes {
        let (record, _) = match agents.changes.record(cx, repo, &id).await {
            Ok(record) => record,
            Err(error) if error.reason == ErrorReason::NotFound => continue,
            Err(error) => return Err(error),
        };
        let cr = record.cr();
        let origin = subject(repo, SubjectKind::ChangeRequest, cr.number.to_string());
        if cr.state() == ChangeState::Landed {
            build.merged.insert(cr.id.clone());
            if let Some(at) = cr.landed_at.as_ref() {
                build.merge_candidates.insert(
                    cr.id.clone(),
                    (cr.revisions.last().map_or(0, |r| r.n), ts_ms(at)),
                );
            }
        }
        // Attribute the opening revision to the run that opened the pull request.
        // A later revision is used only if the run itself recorded its head.
        let revision = cr
            .revisions
            .iter()
            .rev()
            .find(|revision| {
                revision
                    .head
                    .as_ref()
                    .is_some_and(|head| changesets.contains_key(&head.raw))
            })
            .or_else(|| cr.revisions.first());
        let Some(revision) = revision else { continue };
        let Some(head) = revision.head.as_ref() else {
            continue;
        };
        let base = revision.base.as_ref();
        let older_paths;
        let paths = if cr.revisions.last() == Some(revision) {
            for object in &record.stack {
                build.scan_heads.insert(object.raw.clone(), origin.clone());
                changesets.remove(&object.raw);
            }
            &record.paths
        } else if let Some(base) = base {
            let (stack, _, paths, _) = agents.changes.analyze(repo, head, base).await?;
            for object in &stack.members {
                build.scan_heads.insert(object.raw.clone(), origin.clone());
                changesets.remove(&object.raw);
            }
            older_paths = paths;
            &older_paths
        } else {
            continue;
        };
        changesets.remove(&head.raw);
        let time_ms = build.at(&revision.created_at);
        gather_diff(
            agents,
            repo,
            run,
            DiffEvidence {
                paths,
                base,
                head,
                origin: origin.clone(),
                time_ms,
            },
            build,
        )
        .await?;
        check_targets.insert(
            head.raw.clone(),
            (head.clone(), cr.target_ref.clone(), origin),
        );
    }

    let policy = agents.changes.policy_of(repo).await?;
    let (rules, fingerprint) = Changes::class_rules(policy.as_ref())?;
    let has_linked_outcome = !check_targets.is_empty();
    let mut parent_ids = BTreeSet::new();
    for head in changesets.into_values() {
        let vcs = agents.changes.vcs();
        let commit = match vcs.commit(repo, &head).await {
            Ok(commit) => commit,
            Err(error)
                if matches!(
                    error.reason,
                    ErrorReason::NotFound | ErrorReason::MissingObject | ErrorReason::Redacted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        parent_ids.extend(commit.parents.iter().map(|parent| parent.raw.clone()));
        let paths: Vec<PathClass> = vcs
            .classify(repo, std::slice::from_ref(&head), &rules, &fingerprint)
            .await?
            .paths
            .into_iter()
            .map(|path| PathClass {
                path: path.path,
                class: path.class,
                size: path.size.unwrap_or_default(),
                deleted: path.size.is_none(),
            })
            .collect();
        let origin = subject(repo, SubjectKind::Changeset, hex_of(&head));
        gather_diff(
            agents,
            repo,
            run,
            DiffEvidence {
                paths: &paths,
                base: commit.parents.first(),
                head: &head,
                origin: origin.clone(),
                time_ms: run.finished_at_ms.max(creation_ms),
            },
            build,
        )
        .await?;
        // A raw changeset is eligible to be the expected result only if no linked pull
        // request covers the run. Without a PR use the last known work-ref tip below.
        if !has_linked_outcome {
            check_targets.insert(head.raw.clone(), (head, run.run.base_ref.clone(), origin));
        }
    }

    if run.handed_back {
        for (id, (head, target_ref, origin)) in check_targets {
            if !has_linked_outcome && parent_ids.contains(&id) {
                continue;
            }
            let missing = gather_checks(agents, cx, repo, &target_ref, &head).await?;
            if !missing.is_empty() {
                build.signal(
                    SafetySignalKind::SaidVsShown,
                    SafetySeverity::Note,
                    format!(
                        "Reported done with required checks missing or failing: {}",
                        missing.join(", ")
                    ),
                    vec![run_subject.clone(), origin],
                    Vec::new(),
                    run.finished_at_ms.max(creation_ms),
                );
            }
        }
    }
    Ok(())
}

async fn gather_scan_alerts(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    build: &mut Build,
) -> Result<()> {
    use crate::security::{CODE_ALERTS, CODE_INSTANCES, SURFACE as SECURITY, num_id};
    if build.scan_heads.is_empty()
        || !agents
            .forge
            .registry
            .modules()
            .iter()
            .any(|module| module.name == "security")
    {
        return Ok(());
    }
    let head_ids: BTreeSet<_> = build.scan_heads.keys().cloned().collect();
    let mut tx = agents.db.begin(SECURITY, cx).await?;
    let mut cursor = None;
    loop {
        let alerts = tx
            .scan_msgs::<CodeAlertRec>(&CODE_ALERTS, &Scan::scope(&repo.id, 128).after(cursor))
            .await?;
        cursor = alerts.last().map(|(_, row)| Cursor::after(row));
        let done = alerts.len() < 128;
        for (record, _) in alerts {
            let Some(alert) = record.alert else { continue };
            let first = record.first_changeset;
            if first.as_ref().is_some_and(|id| !head_ids.contains(&id.raw)) {
                continue;
            }
            if alert.state() != CodeScanningAlertState::Open {
                continue;
            }
            let scope = format!("{}:{}", repo.id, num_id(alert.number));
            let mut after = None;
            loop {
                let instances = tx
                    .scan_msgs::<InstanceRec>(
                        &CODE_INSTANCES,
                        &Scan::scope(&scope, 128).after(after),
                    )
                    .await?;
                after = instances.last().map(|(_, row)| Cursor::after(row));
                let last = instances.len() < 128;
                for (record, _) in instances {
                    let Some(instance) = record.instance else {
                        continue;
                    };
                    if !detectors::scan_matches(&alert, &instance, &head_ids) {
                        continue;
                    }
                    let Some(origin) = first
                        .as_ref()
                        .or(instance.changeset.as_ref())
                        .and_then(|id| build.scan_heads.get(&id.raw))
                        .cloned()
                    else {
                        continue;
                    };
                    let alert_subject =
                        subject(repo, SubjectKind::SecurityAlert, alert.number.to_string());
                    let paths = instance
                        .location
                        .map(|location| vec![String::from_utf8_lossy(&location.path).into_owned()])
                        .unwrap_or_default();
                    let time_ms = build.at(&alert.updated_at);
                    let severity = match (
                        first.is_some(),
                        alert.rule.as_ref().map(|rule| rule.security_severity()),
                    ) {
                        (false, _) => SafetySeverity::Note,
                        (true, Some(SecuritySeverity::Critical | SecuritySeverity::High)) => {
                            SafetySeverity::Severe
                        }
                        _ => SafetySeverity::Warning,
                    };
                    build.signal(
                        SafetySignalKind::HarmToCode,
                        severity,
                        format!(
                            "Code scanning alert #{} {}",
                            alert.number,
                            if first.is_some() {
                                "was first seen on an attributed revision"
                            } else {
                                "is open on an attributed revision; its first observation is unknown"
                            }
                        ),
                        vec![origin, alert_subject],
                        paths,
                        time_ms,
                    );
                }
                if last {
                    break;
                }
            }
        }
        if done {
            return Ok(());
        }
    }
}

pub(super) async fn collect(
    agents: &Agents,
    cx: &Cx,
    repo: &Repo,
    work: SafetyRef,
) -> Result<Snapshot> {
    let mut build = Build::new(work_ref(repo, work.kind(), &work.id));
    let mut unfinished = false;
    let runs = match work.kind() {
        SafetyWorkKind::Run => {
            let run = run_work(agents, cx, repo, &work.id, None, false).await?;
            build.record.answerable = answerable(&run.rec);
            vec![run]
        }
        SafetyWorkKind::Swarm => {
            let swarm = {
                let mut tx = agents.db.begin(SURFACE, cx).await?;
                swarms::store::get_swarm(&mut tx, &repo.id, &work.id).await?
            };
            let source = subject(repo, SubjectKind::Swarm, &swarm.id);
            build.at(&swarm.updated_at);
            build.record.answerable = swarm.owner.clone().map(public_principal);
            ask(&mut build.record.asked, &swarm.goal, source.clone());
            issue_ask(agents, cx, repo, &swarm.issue, &mut build.record.asked).await?;
            let mut claimed_by_run: BTreeMap<&str, BTreeSet<Vec<u8>>> = BTreeMap::new();
            let mut done_runs = BTreeSet::new();
            for task in &swarm.tasks {
                ask(&mut build.record.asked, &task.title, source.clone());
                for attempt in &task.attempts {
                    claimed_by_run
                        .entry(&attempt.run_id)
                        .or_default()
                        .extend(task.paths.clone());
                    if attempt.outcome() == SwarmAttemptOutcome::Finished
                        && attempt.hand_back.as_ref().is_some_and(|handoff| {
                            handoff.done.iter().any(|item| !item.trim().is_empty())
                        })
                    {
                        done_runs.insert(attempt.run_id.as_str());
                    }
                }
            }
            unfinished = swarm.state() == SwarmState::Finished
                && swarm
                    .tasks
                    .iter()
                    .any(|task| task.state() != SwarmTaskState::Finished);
            let mut runs = Vec::with_capacity(swarm.member_run_ids.len());
            for id in swarm.member_run_ids {
                let claimed = claimed_by_run
                    .remove(id.as_str())
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                runs.push(
                    run_work(
                        agents,
                        cx,
                        repo,
                        &id,
                        Some(claimed),
                        done_runs.contains(id.as_str()),
                    )
                    .await?,
                );
            }
            runs
        }
        SafetyWorkKind::Unspecified => return Err(ForgeError::missing("work.kind")),
    };
    for run in runs {
        gather_run(agents, cx, repo, &run, &mut build).await?;
    }
    gather_scan_alerts(agents, cx, repo, &mut build).await?;
    let agent_merge_times = agent_merges(agents, cx, repo, &build.merge_candidates).await?;
    Ok(build.finish(unfinished, agent_merge_times))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_human_approval_belongs_to_the_landed_revision() {
        let mut review = Review {
            reviewer: Some(Principal {
                kind: PrincipalKind::Human as i32,
                ..Default::default()
            }),
            verdict: Verdict::Approve as i32,
            revision: 1,
            created_at: Some(ts(10)),
            ..Default::default()
        };
        assert!(human_approved(std::slice::from_ref(&review), 1, 20));
        assert!(!human_approved(std::slice::from_ref(&review), 2, 20));
        assert!(!human_approved(std::slice::from_ref(&review), 1, 9));
        review.dismissed = true;
        assert!(!human_approved(&[review], 1, 20));
    }
}
