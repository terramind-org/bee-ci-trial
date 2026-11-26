//! Forge events → workflow events (`docs/spec/workflows.md` §5 and §8).
//!
//! [`CiListener`] receives every appended forge event and starts the workflows it triggers:
//! `ref.updated` becomes `push` (plus `create`/`delete`; `before` is the last tip CI saw, the
//! changed paths come from the VCS diff), `cr.*` events become `pull_request` and
//! `pull_request_target` activities, `review.submitted` and `review.dismissed` become
//! `pull_request_review` `submitted` and `dismissed` (the change request is read from the
//! changes store, since review events carry only its id),
//! `issue.*` and `issue_comment.*` become `issues` and `issue_comment`, `release.published`
//! becomes `release`, `merge_queue.checks_requested` becomes `merge_group` on the queue
//! entry's combined result, and other events with a GitHub name become that event. Events caused
//! by a job token (principal `job:<id>`) start no workflows except dispatches. Schedules run
//! on a minute tick the server drives ([`Ci::tick_schedule`]); manual runs go through
//! [`Ci::dispatch`] and [`Ci::repository_dispatch`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{ChangeRequest, Event, ObjectId, PrincipalKind, Repo, WorkflowState, event};
use bee_workflow::cron::UtcMinute;
use bee_workflow::event::{
    BeeEvent, Cause, ChangeRequestEvent, ChangedPaths, CrAction, MergeQueueEvent, PushEvent,
};
use bee_workflow::{Value, parse_workflow};

use super::convert::{bee_ref, git_ref};
use super::engine::{Ci, TriggerContext, object_id};
use super::model::{RefTip, WorkflowRow};
use super::{COUNTERS, REF_TIPS, SURFACE, WORKFLOWS};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::EventListener;
use crate::ids::hex;
use crate::module::Registry;
use crate::store::{Key, Row, Scan};

const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Starts workflows from forge events.
pub struct CiListener {
    ci: Arc<Ci>,
    registry: Option<Arc<Registry>>,
}

impl CiListener {
    /// A listener for `ci`.
    pub fn new(ci: Arc<Ci>) -> CiListener {
        CiListener { ci, registry: None }
    }

    /// A listener that also maps other GitHub-named event kinds of `registry`.
    pub fn with_registry(ci: Arc<Ci>, registry: Arc<Registry>) -> CiListener {
        CiListener {
            ci,
            registry: Some(registry),
        }
    }
}

#[async_trait]
impl EventListener for CiListener {
    async fn on_event(&self, ev: &Event) {
        let _ = self.ci.handle_event(ev, self.registry.as_deref()).await;
    }
}

fn cr_action(kind: &str) -> Option<CrAction> {
    Some(match kind {
        "cr.opened" => CrAction::Opened,
        "cr.revised" => CrAction::Revised,
        "cr.landed" => CrAction::Landed,
        "cr.closed" => CrAction::Closed,
        "cr.reopened" => CrAction::Reopened,
        "cr.edited" => CrAction::Edited,
        "cr.ready_for_review" => CrAction::ReadyForReview,
        "cr.converted_to_draft" => CrAction::ConvertedToDraft,
        "cr.labels_changed" => CrAction::Labeled,
        "cr.assignees_changed" => CrAction::Assigned,
        "cr.review_requested" => CrAction::ReviewRequested,
        "cr.review_request_removed" => CrAction::ReviewRequestRemoved,
        "cr.locked" => CrAction::Locked,
        "cr.unlocked" => CrAction::Unlocked,
        "cr.milestone_changed" => CrAction::Milestoned,
        "cr.auto_merge_enabled" => CrAction::AutoMergeEnabled,
        "cr.auto_merge_disabled" => CrAction::AutoMergeDisabled,
        "merge_queue.entered" => CrAction::Enqueued,
        "merge_queue.left" => CrAction::Dequeued,
        _ => return None,
    })
}

fn num(v: &serde_json::Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn label_names(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl Ci {
    /// Starts the workflows one forge event triggers; returns the new run ids.
    pub async fn handle_event(
        &self,
        ev: &Event,
        registry: Option<&Registry>,
    ) -> Result<Vec<String>> {
        if ev.repo_id.is_empty() {
            return Ok(Vec::new());
        }
        let cx = Cx::system();
        let by_token = ev.principal.starts_with("job:");
        let cause = Cause {
            actor: ev.principal.clone(),
            by_workflow_token: by_token,
        };
        let repo = match self.repo_by_id(&cx, &ev.repo_id).await {
            Ok(r) if r.deleted_at.is_none() && !r.archived => r,
            _ => return Ok(Vec::new()),
        };
        let actor = Some(bee_proto::v1::Principal {
            id: ev.principal.clone(),
            ..Default::default()
        });
        let kind = ev.kind.as_str();
        if kind == "ref.updated" {
            let Some(event::Body::RefUpdated(r)) = &ev.body else {
                return Ok(Vec::new());
            };
            return self.on_ref_updated(&cx, &repo, r, cause, actor).await;
        }
        if kind == "merge_queue.checks_requested" {
            return Box::pin(self.on_merge_queue(&cx, &repo, ev, cause, actor)).await;
        }
        if let Some(action) = cr_action(kind) {
            let Some(event::Body::Change(cr)) = &ev.body else {
                return Ok(Vec::new());
            };
            let (cre, ctx) = self.cr_event(&repo, cr, action, actor).await?;
            return self
                .trigger(&cx, &repo, &BeeEvent::ChangeRequest(cre), &cause, &ctx)
                .await;
        }
        if let ("review.submitted" | "review.dismissed", Some(event::Body::Review(r))) =
            (kind, &ev.body)
        {
            let cr = {
                let mut tx = self.db.begin(crate::changes::SURFACE, &cx).await?;
                crate::changes::Changes::load(&mut tx, &repo.id, &r.change_id)
                    .await?
                    .0
                    .cr()
                    .clone()
            };
            let (cre, ctx) = self.cr_event(&repo, &cr, CrAction::Edited, actor).await?;
            let state = if kind == "review.dismissed" {
                "dismissed"
            } else {
                match bee_proto::v1::Verdict::try_from(r.verdict)
                    .unwrap_or(bee_proto::v1::Verdict::Unspecified)
                {
                    bee_proto::v1::Verdict::Approve => "approved",
                    bee_proto::v1::Verdict::RequestChanges => "changes_requested",
                    _ => "commented",
                }
            };
            let review = BeeEvent::Review {
                action: kind.trim_start_matches("review.").to_string(),
                cr: cre,
                state: state.to_string(),
            };
            return self.trigger(&cx, &repo, &review, &cause, &ctx).await;
        }
        let payload: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap_or_default();
        let ctx = TriggerContext {
            actor,
            ..Default::default()
        };
        let bee = if let Some(action) = kind.strip_prefix("issue.") {
            if action == "assigned_to_agent" {
                return Ok(Vec::new());
            }
            BeeEvent::Issue {
                action: action.to_string(),
                number: num(&payload["number"]),
                title: payload["title"].as_str().unwrap_or("").to_string(),
                labels: label_names(&payload["labels"]),
            }
        } else if let Some(action) = kind.strip_prefix("issue_comment.") {
            BeeEvent::IssueComment {
                action: action.to_string(),
                number: num(&payload["issueNumber"]).max(num(&payload["issue_number"])),
                body: payload["body"].as_str().unwrap_or("").to_string(),
                on_change_request: false,
            }
        } else if kind == "cr_comment.created" {
            BeeEvent::IssueComment {
                action: "created".into(),
                number: num(&payload["number"]),
                body: payload["body"].as_str().unwrap_or("").to_string(),
                on_change_request: true,
            }
        } else if let Some(action) = kind.strip_prefix("release.") {
            BeeEvent::Release {
                action: action.to_string(),
                tag: payload["tagName"]
                    .as_str()
                    .or(payload["tag_name"].as_str())
                    .unwrap_or("")
                    .to_string(),
                sha: String::new(),
                name: payload["name"].as_str().unwrap_or("").to_string(),
                prerelease: payload["prerelease"].as_bool().unwrap_or(false),
            }
        } else {
            let Some(reg) = registry else {
                return Ok(Vec::new());
            };
            let Some(k) = reg.event_kind(kind) else {
                return Ok(Vec::new());
            };
            let Some((name, action)) = k.github else {
                return Ok(Vec::new());
            };
            if matches!(
                name,
                "workflow_run"
                    | "workflow_job"
                    | "check_run"
                    | "check_suite"
                    | "status"
                    | "deployment"
                    | "deployment_status"
                    | "deployment_review"
                    | "repository_dispatch"
                    | "pull_request"
                    | "pull_request_review"
                    | "push"
                    | "issues"
                    | "issue_comment"
            ) {
                return Ok(Vec::new());
            }
            BeeEvent::Other {
                name: name.to_string(),
                action: (!action.is_empty()).then(|| action.to_string()),
                payload: super::util::from_json(&payload),
            }
        };
        self.trigger(&cx, &repo, &bee, &cause, &ctx).await
    }

    /// `merge_queue.checks_requested` → `merge_group` (`checks_requested`) on the queue
    /// entry's combined result; its check runs are reported on that changeset.
    async fn on_merge_queue(
        &self,
        cx: &Cx,
        repo: &Repo,
        ev: &Event,
        cause: Cause,
        actor: Option<bee_proto::v1::Principal>,
    ) -> Result<Vec<String>> {
        let p: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap_or_default();
        let field = |k: &str| p[k].as_str().unwrap_or_default().to_string();
        let head_sha = field("head_sha");
        let Some(head) = object_id(&head_sha) else {
            return Ok(Vec::new());
        };
        let base_ref = bee_ref(&field("base_ref"));
        let head_ref = field("head_ref");
        let mq = MergeQueueEvent {
            base_ref: base_ref.clone(),
            base_sha: field("base_sha"),
            head_ref: git_ref(&head_ref),
            head_sha,
            head_commit_message: field("head_commit_message"),
        };
        let (numbers, ids) = match &ev.body {
            Some(event::Body::Change(cr)) => (vec![cr.number], vec![cr.id.clone()]),
            _ => (Vec::new(), Vec::new()),
        };
        let ctx = TriggerContext {
            change_request_numbers: numbers,
            change_request_ids: ids,
            head_ref,
            base_ref,
            check_changeset: Some(head),
            actor,
            ..Default::default()
        };
        self.trigger(cx, repo, &BeeEvent::MergeQueue(mq), &cause, &ctx)
            .await
    }

    async fn cr_event(
        &self,
        repo: &Repo,
        cr: &ChangeRequest,
        action: CrAction,
        actor: Option<bee_proto::v1::Principal>,
    ) -> Result<(ChangeRequestEvent, TriggerContext)> {
        let rev = cr.revisions.last().cloned().unwrap_or_default();
        let head = rev.head.clone().unwrap_or_default();
        let target = bee_ref(if cr.target_ref.is_empty() {
            &repo.default_ref
        } else {
            &cr.target_ref
        });
        let base_tip = self.vcs.ref_tip(repo, &target).await?;
        let base = rev.base.clone().or_else(|| base_tip.clone());
        let changed = match &base {
            Some(b) if !head.raw.is_empty() => self
                .vcs
                .changed_paths(repo, Some(b), &head)
                .await
                .map(ChangedPaths::Known)
                .unwrap_or(ChangedPaths::Unknown),
            _ => ChangedPaths::Unknown,
        };
        let message = if head.raw.is_empty() {
            None
        } else {
            self.vcs.message(repo, &head).await.ok()
        };
        let labels: Vec<String> = cr.labels.iter().map(|l| l.name.clone()).collect();
        let head_ref = format!("cr-{}", cr.number);
        let from_fork = !cr.head_repo_id.is_empty() && cr.head_repo_id != repo.id;
        let e = ChangeRequestEvent {
            action,
            number: cr.number,
            title: cr.title.clone(),
            base_ref: target.clone(),
            base_sha: base_tip.as_ref().map(|b| hex(&b.raw)).unwrap_or_default(),
            head_ref: head_ref.clone(),
            head_sha: hex(&head.raw),
            merge_sha: None,
            has_conflicts: false,
            draft: cr.draft,
            from_fork,
            labels: labels.clone(),
            label: if action == CrAction::Labeled {
                labels.last().cloned()
            } else {
                None
            },
            head_commit_message: message,
            changed,
        };
        let ctx = TriggerContext {
            change_request_numbers: vec![cr.number],
            change_request_ids: vec![cr.id.clone()],
            head_ref,
            base_ref: target,
            check_changeset: (!head.raw.is_empty()).then(|| head.clone()),
            agent_author: cr
                .author
                .as_ref()
                .is_some_and(|a| a.kind == PrincipalKind::Agent as i32),
            from_fork,
            author: cr.author.clone(),
            actor,
            chain_depth: 0,
            probe_jobs: Vec::new(),
        };
        Ok((e, ctx))
    }

    async fn on_ref_updated(
        &self,
        cx: &Cx,
        repo: &Repo,
        r: &bee_proto::v1::Ref,
        cause: Cause,
        actor: Option<bee_proto::v1::Principal>,
    ) -> Result<Vec<String>> {
        // `green/*` marks tested tips (SPEC §5); CI writes it and never runs workflows for it.
        if r.name.starts_with("green/") {
            return Ok(Vec::new());
        }
        let after = r.target.clone().filter(|t| !t.raw.is_empty());
        let before = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let prev = tx
                .get_msg::<RefTip>(&REF_TIPS, &repo.id, &r.name)
                .await?
                .map(|(t, _)| t.target)
                .filter(|t| !t.is_empty());
            let tip = RefTip {
                target: after.as_ref().map(|a| hex(&a.raw)).unwrap_or_default(),
            };
            if prev.as_deref() == Some(tip.target.as_str()) {
                return Ok(Vec::new());
            }
            tx.put(
                &REF_TIPS,
                &Row::new(repo.id.clone(), r.name.clone()).msg(&tip),
            )
            .await?;
            tx.commit().await?;
            prev
        };
        let default = bee_ref(if repo.default_ref.is_empty() {
            "main"
        } else {
            &repo.default_ref
        });
        if r.name == default
            && let Some(a) = &after
        {
            let _ = self.refresh_workflows(cx, repo, a, &git_ref(&r.name)).await;
        }
        let before_id = before.as_deref().and_then(object_id);
        let created = before.is_none();
        let deleted = after.is_none();
        let (changed, count, forced, message) = match &after {
            Some(a) => {
                let base = match &before_id {
                    Some(b) => Some(b.clone()),
                    None => self.vcs.ref_tip(repo, &default).await?.filter(|d| d != a),
                };
                let changed = self
                    .vcs
                    .changed_paths(repo, base.as_ref(), a)
                    .await
                    .map(ChangedPaths::Known)
                    .unwrap_or(ChangedPaths::Unknown);
                let forced = match &before_id {
                    Some(b) => !self.vcs.is_ancestor(repo, b, a).await.unwrap_or(true),
                    None => false,
                };
                let count = self
                    .vcs
                    .count_between(repo, before_id.as_ref(), a, 1000)
                    .await
                    .unwrap_or(1);
                (changed, count, forced, self.vcs.message(repo, a).await.ok())
            }
            None => (ChangedPaths::Known(Vec::new()), 0, false, None),
        };
        let push = PushEvent {
            git_ref: git_ref(&r.name),
            before: before.clone().unwrap_or_else(|| ZEROS.to_string()),
            after: after
                .as_ref()
                .map(|a| hex(&a.raw))
                .unwrap_or_else(|| ZEROS.to_string()),
            created,
            deleted,
            forced,
            commit_count: count,
            head_commit_message: message,
            changed,
        };
        let ctx = TriggerContext {
            head_ref: r.name.clone(),
            actor,
            ..Default::default()
        };
        self.trigger(cx, repo, &BeeEvent::Push(push), &cause, &ctx)
            .await
    }

    /// Re-reads the workflows of the default ref at `tip`: new and changed files are recorded,
    /// files that disappeared are marked deleted.
    pub async fn refresh_workflows(
        &self,
        cx: &Cx,
        repo: &Repo,
        tip: &ObjectId,
        git: &str,
    ) -> Result<()> {
        let files = self.workflow_files(repo, tip).await?;
        for (path, src) in &files {
            let wf = parse_workflow(path, src).ok();
            self.ensure_workflow(cx, repo, path, wf.as_ref(), Some(tip), git)
                .await?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for (mut w, row) in tx
            .scan_msgs::<WorkflowRow>(&WORKFLOWS, &Scan::scope(repo.id.clone(), 1000))
            .await?
        {
            let mut wf = w.workflow.clone().unwrap_or_default();
            if !files.contains_key(&wf.path) && wf.state != WorkflowState::Deleted as i32 {
                wf.state = WorkflowState::Deleted as i32;
                w.workflow = Some(wf.clone());
                w.schedules.clear();
                tx.update(
                    &WORKFLOWS,
                    &Row::new(repo.id.clone(), row.id.clone())
                        .k1(wf.path.clone())
                        .version(row.version)
                        .msg(&w),
                )
                .await?;
            }
        }
        tx.commit().await
    }

    /// Runs `schedule` workflows whose cron matches `minute` (each minute once). The server
    /// drives this from its clock ([`Ci::spawn_worker`] does it once a minute).
    pub async fn tick_schedule(&self, cx: &Cx, minute: UtcMinute) -> Result<Vec<String>> {
        let stamp = (((i64::from(minute.year) * 13 + i64::from(minute.month)) * 32
            + i64::from(minute.day))
            * 24
            + i64::from(minute.hour))
            * 60
            + i64::from(minute.minute);
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.add_n1(&COUNTERS, "", "schedule_tick", 0).await?;
            let last = tx
                .get(&COUNTERS, "", "schedule_minute")
                .await?
                .and_then(|r| r.n1)
                .unwrap_or(0);
            if last >= stamp {
                return Ok(Vec::new());
            }
            tx.put(&COUNTERS, &Row::new("", "schedule_minute").n1(stamp))
                .await?;
            tx.commit().await?;
        }
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &WORKFLOWS,
                &Scan {
                    key: Some(Key::K2("schedule".into())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        };
        let mut repos: BTreeSet<String> = BTreeSet::new();
        for r in &rows {
            let w: WorkflowRow = r.decode()?;
            if w.workflow
                .as_ref()
                .is_some_and(|x| x.state == WorkflowState::Active as i32)
            {
                repos.insert(r.scope.clone());
            }
        }
        let mut out = Vec::new();
        for repo_id in repos {
            let Ok(repo) = self.repo_by_id(cx, &repo_id).await else {
                continue;
            };
            if repo.archived || repo.deleted_at.is_some() {
                continue;
            }
            let cause = Cause {
                actor: "beehive-scheduler".into(),
                by_workflow_token: false,
            };
            if let Ok(ids) = self
                .trigger(
                    cx,
                    &repo,
                    &BeeEvent::ScheduleTick(minute),
                    &cause,
                    &TriggerContext::default(),
                )
                .await
            {
                out.extend(ids);
            }
        }
        Ok(out)
    }

    /// Finds a workflow row by id, path or file name.
    pub async fn find_workflow(&self, cx: &Cx, repo: &Repo, name: &str) -> Result<WorkflowRow> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if let Some((w, _)) = tx
            .get_msg::<WorkflowRow>(&WORKFLOWS, &repo.id, name)
            .await?
        {
            return Ok(w);
        }
        let mut candidates = vec![name.to_string()];
        if !name.contains('/') {
            for d in super::engine::WORKFLOW_DIRS {
                candidates.push(format!("{d}/{name}"));
            }
        }
        for c in candidates {
            if let Some(r) = tx
                .scan(&WORKFLOWS, &Scan::scope(repo.id.clone(), 1).key(Key::K1(c)))
                .await?
                .first()
            {
                return r.decode();
            }
        }
        Err(ForgeError::not_found(format!("workflow {name}")))
    }

    /// Starts a `workflow_dispatch` run of one workflow at `r` (default ref when empty).
    pub async fn dispatch(
        &self,
        cx: &Cx,
        repo: &Repo,
        workflow: &str,
        r: &str,
        inputs: BTreeMap<String, String>,
    ) -> Result<(String, u64)> {
        let ref_name = bee_ref(if r.is_empty() {
            if repo.default_ref.is_empty() {
                "main"
            } else {
                &repo.default_ref
            }
        } else {
            r
        });
        let tip = self
            .vcs
            .ref_tip(repo, &ref_name)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("ref {ref_name}")))?;
        let path = match self.find_workflow(cx, repo, workflow).await {
            Ok(w) => w.workflow.unwrap_or_default().path,
            Err(_) => {
                let files = self.workflow_files(repo, &tip).await?;
                files
                    .keys()
                    .find(|p| *p == workflow || p.rsplit('/').next() == Some(workflow))
                    .cloned()
                    .ok_or_else(|| ForgeError::not_found(format!("workflow {workflow}")))?
            }
        };
        if let Ok(w) = self.find_workflow(cx, repo, &path).await
            && w.workflow.as_ref().is_some_and(|x| {
                x.state != WorkflowState::Active as i32 && x.state != WorkflowState::Deleted as i32
            })
        {
            return Err(ForgeError::invalid_state(format!(
                "workflow {path} is disabled"
            )));
        }
        let src = self
            .vcs
            .read_file(repo, &tip, &path)
            .await?
            .ok_or_else(|| {
                ForgeError::invalid_state(format!("{path} does not exist at {ref_name}"))
            })?;
        let wf = parse_workflow(&path, &String::from_utf8_lossy(&src))
            .map_err(|e| ForgeError::invalid("invalid_workflow", e.to_string()))?;
        if wf.trigger("workflow_dispatch").is_none() {
            return Err(ForgeError::invalid_state(format!(
                "{path} has no workflow_dispatch trigger at {ref_name}"
            )));
        }
        bee_workflow::event::dispatch_inputs(&wf, &inputs)
            .map_err(|m| ForgeError::invalid("bad_input", m))?;
        let ev = BeeEvent::WorkflowDispatch {
            workflow: path.clone(),
            git_ref: git_ref(&ref_name),
            sha: hex(&tip.raw),
            inputs,
        };
        let cause = Cause {
            actor: cx.actor_id.clone(),
            by_workflow_token: cx.actor_id.starts_with("job:"),
        };
        let ctx = TriggerContext {
            head_ref: ref_name,
            actor: Some(cx.principal()),
            ..Default::default()
        };
        let ids = self.trigger(cx, repo, &ev, &cause, &ctx).await?;
        let id = ids.first().cloned().ok_or_else(|| {
            ForgeError::invalid_state(format!("{path} did not start (actions disabled?)"))
        })?;
        let run = self.get_run(cx, &repo.id, &id).await?;
        Ok((id, run.run_number))
    }

    /// Fires `repository_dispatch` for every workflow listening for `event_type`.
    pub async fn repository_dispatch(
        &self,
        cx: &Cx,
        repo: &Repo,
        event_type: &str,
        client_payload_json: &str,
    ) -> Result<Vec<String>> {
        if event_type.is_empty() || event_type.len() > 100 {
            return Err(ForgeError::invalid(
                "invalid_field",
                "event_type must be 1-100 characters",
            ));
        }
        let payload: serde_json::Value = if client_payload_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(client_payload_json)
                .map_err(|e| ForgeError::invalid("bad_json", format!("client_payload_json: {e}")))?
        };
        let Some(obj) = payload.as_object() else {
            return Err(ForgeError::invalid(
                "bad_json",
                "client_payload_json must be a JSON object",
            ));
        };
        if obj.len() > 10 {
            return Err(ForgeError::too_large(
                "client_payload_keys",
                10,
                obj.len() as u64,
            ));
        }
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.emit_event(Event {
                kind: "repository_dispatch.created".into(),
                repo_id: repo.id.clone(),
                org_id: repo.org_id.clone(),
                payload_json:
                    serde_json::json!({ "action": event_type, "client_payload": payload })
                        .to_string(),
                ..Default::default()
            })
            .await?;
            tx.audit(
                AuditDraft::new("workflow.repository_dispatch", &repo.org_id)
                    .repo_id(&repo.id)
                    .after(&serde_json::json!({ "event_type": event_type }).to_string()),
            )
            .await?;
            tx.commit().await?;
        }
        let ev = BeeEvent::RepositoryDispatch {
            event_type: event_type.to_string(),
            client_payload: super::util::from_json(&payload),
        };
        let cause = Cause {
            actor: cx.actor_id.clone(),
            by_workflow_token: cx.actor_id.starts_with("job:"),
        };
        let ctx = TriggerContext {
            actor: Some(cx.principal()),
            ..Default::default()
        };
        let _ = Value::Null;
        self.trigger(cx, repo, &ev, &cause, &ctx).await
    }

    /// Enables or disables a workflow.
    pub async fn set_workflow_state(
        &self,
        cx: &Cx,
        repo: &Repo,
        workflow: &str,
        enable: bool,
    ) -> Result<bee_proto::v1::Workflow> {
        let w = self.find_workflow(cx, repo, workflow).await?;
        let id = w.workflow.clone().unwrap_or_default().id;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let Some((mut row, r)) = tx.get_msg::<WorkflowRow>(&WORKFLOWS, &repo.id, &id).await? else {
            return Err(ForgeError::not_found(format!("workflow {workflow}")));
        };
        let mut wf = row.workflow.clone().unwrap_or_default();
        wf.state = if enable {
            WorkflowState::Active
        } else {
            WorkflowState::DisabledManually
        } as i32;
        wf.updated_at = Some(crate::ids::ts(tx.now_ms()));
        row.workflow = Some(wf.clone());
        let mut nr = Row::new(repo.id.clone(), id.clone())
            .k1(wf.path.clone())
            .version(r.version)
            .msg(&row);
        if !row.schedules.is_empty() {
            nr = nr.k2("schedule");
        }
        tx.update(&WORKFLOWS, &nr).await?;
        tx.emit_event(Event {
            kind: "workflow.updated".into(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            payload_json: serde_json::to_string(&wf).unwrap_or_default(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new(
                if enable {
                    "workflow.enable"
                } else {
                    "workflow.disable"
                },
                &repo.org_id,
            )
            .repo_id(&repo.id)
            .target_id(&id),
        )
        .await?;
        tx.commit().await?;
        Ok(wf)
    }

    /// Workflows of a repo at `r` (the default ref when empty), refreshed from that ref.
    pub async fn list_workflows(
        &self,
        cx: &Cx,
        repo: &Repo,
        r: &str,
    ) -> Result<Vec<bee_proto::v1::Workflow>> {
        let ref_name = bee_ref(if r.is_empty() {
            if repo.default_ref.is_empty() {
                "main"
            } else {
                &repo.default_ref
            }
        } else {
            r
        });
        let tip = self.vcs.ref_tip(repo, &ref_name).await?;
        let default = bee_ref(if repo.default_ref.is_empty() {
            "main"
        } else {
            &repo.default_ref
        });
        let mut paths: Option<BTreeSet<String>> = None;
        if let Some(t) = &tip {
            if ref_name == default {
                self.refresh_workflows(cx, repo, t, &git_ref(&ref_name))
                    .await?;
            } else {
                let files = self.workflow_files(repo, t).await?;
                for (p, s) in &files {
                    let wf = parse_workflow(p, s).ok();
                    self.ensure_workflow(cx, repo, p, wf.as_ref(), None, "")
                        .await?;
                }
                paths = Some(files.keys().cloned().collect());
            }
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut v: Vec<bee_proto::v1::Workflow> = tx
            .scan_msgs::<WorkflowRow>(&WORKFLOWS, &Scan::scope(repo.id.clone(), 1000))
            .await?
            .into_iter()
            .filter_map(|(w, _)| w.workflow)
            .filter(|w| paths.as_ref().is_none_or(|p| p.contains(&w.path)))
            .collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(v)
    }
}
