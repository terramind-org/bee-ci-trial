//! Landing: policy decisions, landing through the [`super::Lander`], auto-merge, the merge
//! queue, land conflicts, revert, "update base" and the repo context API.
//!
//! **The server owns landing** (SPEC §7.4). This module supplies the decision
//! ([`Changes::evaluate`]: `bee_policy::Policy::evaluate_land` over the CR's current
//! revision, its reviews, the latest check per name of its head
//! ([`crate::checks::Checks::check_results`]) and the ref's live leases
//! ([`crate::leases::Leases::policy_leases`])), hands allowed lands to the installed
//! [`super::Lander`], and records the outcome ([`Changes::record_landed`]: `cr.landed` with
//! the `ChangeRequest` body, so leases auto-release).
//!
//! **Auto-merge** is a land intent stored on the CR. [`AutoMergeTrigger`] re-evaluates it on
//! `check.updated` (for the head the check ran on), `review.submitted` / `review.dismissed`,
//! `cr.ready_for_review`, `cr.revised`, and for children retargeted by a parent's `cr.landed`.
//!
//! **Merge queue.** The queue is persisted here (order: priority, then arrival); the server's
//! coalescer takes entries in order ([`Changes::queue`]), reports their state
//! ([`Changes::set_queue_state`]) and lands them ([`Changes::land_queued`], which
//! [`Changes::process_queue`] runs as a reference single-step coalescer).
//! [`Changes::step_queue`] is what the server's queue worker runs; with the repo setting
//! `merge_queue_check_combined` it tests each entry's combined result on the tip before landing
//! exactly that result.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use bee_policy::{Actor, LandContext};
use bee_proto::v1::{
    AutoMerge, ChangeRequest, ChangeState, CheckStatus, ContextInstruction, ErrorReason, Event,
    LandConflict, LandConflictState, LandMethod, LandResponse, LandingEvaluation, MergeQueueEntry,
    MergeQueueState, ObjectId, PathOwners, PrincipalKind, Ref, Repo, RepoContext, SubjectKind,
    Verdict, event,
};

use super::model::CrRecord;
use super::store::{
    Changes, NewChange, cr_json, current_rev, head_of, hex_of, land_rule, lock_change_request,
    policy_kind, require_open,
};
use super::vcs::{LandIntent, NewCommit, Rebased, merge_conflict};
use super::{CONFLICTS, CRS, LANDED_BY, QUEUE, QUEUE_LOG, SURFACE};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::EventListener;
use crate::ids::{ts, ts_ms};
use crate::store::{Cursor, Key, Row, Scan, Tx};

/// The policy used when the repo has none: anyone the forge roles allow may land.
pub const DEFAULT_POLICY: &str = r#"version = 1

[members]
write = ["@everyone"]

[[access]]
path = "/"
read = ["@everyone"]
write = ["@everyone"]
"#;

/// Most bytes of one agent instruction file returned by the context API.
pub const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;
/// Window for merge-queue throughput.
const THROUGHPUT_WINDOW_MS: u64 = 10 * 60 * 1000;

/// What one [`Changes::step_queue`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum QueueStep {
    /// The queue is empty.
    Empty,
    /// The head entry waits for checks (on its head, or on its combined result).
    Waiting,
    /// The head entry landed.
    Landed(Box<ChangeRequest>),
    /// The head entry was ejected: its CR id and the reason (an `ErrorReason` name).
    Ejected(String, String),
}

/// A land decision with what went into it.
pub(crate) struct Decision {
    pub decision: bee_policy::LandDecision,
    pub draft: bool,
    pub parent_open: Option<u64>,
}

fn method_of(m: i32) -> LandMethod {
    LandMethod::try_from(m).unwrap_or(LandMethod::Unspecified)
}

fn order_key(priority: u32, seq: i64) -> i64 {
    (1000 - i64::from(priority.min(999))) * 1_000_000_000_000 + seq
}

impl Changes {
    // ----- decisions ------------------------------------------------------------------------

    /// A policy actor for a principal id (kind and aliases from the directory).
    async fn policy_actor(&self, id: &str, kind_hint: Option<PrincipalKind>) -> Actor {
        let p = self.directory.get(id).await;
        let kind = match (kind_hint, &p) {
            (Some(k), _) if k != PrincipalKind::Unspecified => k as i32,
            (_, Some(p)) => p.kind,
            _ => PrincipalKind::Human as i32,
        };
        let (logins, emails) = self.directory.identities(id).await;
        Actor::new(id, policy_kind(kind)).with_aliases(logins.into_iter().chain(emails))
    }

    /// The repo's policy, or [`DEFAULT_POLICY`].
    pub(crate) async fn effective_policy(&self, repo: &Repo) -> Result<bee_policy::Policy> {
        match self.policy_of(repo).await? {
            Some(p) => Ok(p),
            None => bee_policy::Policy::parse(DEFAULT_POLICY)
                .map_err(|e| ForgeError::internal(format!("default policy: {e}"))),
        }
    }

    /// Evaluates landing the CR's current revision for the principals in `chain` (actor
    /// first; the first one's kind is `actor_kind`).
    pub(crate) async fn decide(
        &self,
        cx: &Cx,
        repo: &Repo,
        rec: &CrRecord,
        chain: &[String],
        actor_kind: PrincipalKind,
    ) -> Result<Decision> {
        self.decide_at(cx, repo, rec, chain, actor_kind, None).await
    }

    /// [`Changes::decide`] with the checks (and required workflows) read from `checks_at`
    /// instead of the CR's head: the merge queue's combined result.
    pub(crate) async fn decide_at(
        &self,
        cx: &Cx,
        repo: &Repo,
        rec: &CrRecord,
        chain: &[String],
        actor_kind: PrincipalKind,
        checks_at: Option<&ObjectId>,
    ) -> Result<Decision> {
        let cr = rec.cr();
        let policy = self.effective_policy(repo).await?;
        let mut actors = Vec::with_capacity(chain.len());
        for (i, id) in chain.iter().enumerate() {
            // Each principal of the chain carries its own repo role as `@repo-role/<role>`
            // (SPEC §9.4), as the server's land evaluation does.
            let perm = self.orgs.effective_role(cx, repo, id).await?;
            let role = bee_proto::v1::RepoRole::try_from(perm.role)
                .unwrap_or(bee_proto::v1::RepoRole::None);
            actors.push(
                self.policy_actor(id, (i == 0).then_some(actor_kind))
                    .await
                    .with_repo_role(crate::orgs::role_name(role)),
            );
        }
        let head = head_of(cr).cloned().unwrap_or_default();
        let head_hex = hex_of(&head);
        let mut reviews = Vec::new();
        for r in self.raw_reviews(cx, &cr.id).await? {
            if r.dismissed {
                continue;
            }
            let Some(p) = &r.reviewer else { continue };
            let revision = cr
                .revisions
                .get(r.revision.saturating_sub(1) as usize)
                .and_then(|rv| rv.head.as_ref())
                .map(hex_of)
                .unwrap_or_default();
            let verdict = match Verdict::try_from(r.verdict).unwrap_or(Verdict::Comment) {
                Verdict::Approve => bee_policy::Verdict::Approve,
                Verdict::RequestChanges => bee_policy::Verdict::RequestChanges,
                _ => bee_policy::Verdict::Comment,
            };
            reviews.push(bee_policy::Review {
                reviewer: self
                    .policy_actor(
                        &p.id,
                        Some(PrincipalKind::try_from(p.kind).unwrap_or(PrincipalKind::Human)),
                    )
                    .await,
                revision,
                verdict,
            });
        }
        let checks_hex = checks_at.map(hex_of).unwrap_or_else(|| head_hex.clone());
        let checks = self.checks.check_results(cx, &repo.id, &checks_hex).await?;
        let leases = match self
            .leases
            .policy_leases(cx, &repo.id, &cr.target_ref)
            .await
        {
            Ok(l) => l,
            Err(e) if e.retryable() => Vec::new(),
            Err(e) => return Err(e),
        };
        let facts = self.land_facts();
        let heads: Vec<ObjectId> = match checks_at {
            Some(s) => vec![s.clone()],
            None => head_of(cr).cloned().into_iter().collect(),
        };
        let mut signatures = vec![bee_policy::SignatureState::Missing; rec.stack.len()];
        let mut required_workflows = Vec::new();
        let mut code_alerts = Vec::new();
        if let Some(f) = &facts {
            if policy.requires_signatures(&cr.target_ref) {
                signatures = f.signatures(repo, &rec.stack).await?;
            }
            required_workflows = f.required_workflows(repo, &heads).await?;
            if policy.requires_code_scanning(&cr.target_ref) {
                code_alerts = f.code_alerts(repo, &cr.target_ref, &heads).await?;
            }
        }
        let ctx = LandContext {
            chain: actors,
            ref_name: cr.target_ref.clone(),
            changes: rec
                .paths
                .iter()
                .map(|p| bee_policy::ChangedPath {
                    path: p.path.clone(),
                    class: bee_policy::DiffClass::parse(&p.class)
                        .unwrap_or(bee_policy::DiffClass::Logic),
                    size: (!p.deleted).then_some(p.size),
                })
                .collect(),
            head_revision: head_hex,
            last_pusher: rec
                .revs
                .last()
                .and_then(|r| r.pusher.as_ref())
                .map(|p| bee_policy::PrincipalId::new(p.id.clone())),
            reviews,
            checks,
            leases,
            commits: rec
                .stack
                .iter()
                .zip(signatures)
                .map(|(s, signature)| bee_policy::CommitInfo {
                    id: hex_of(s),
                    signature,
                })
                .collect(),
            has_merges: rec.has_merges,
            required_workflows,
            code_alerts,
        };
        let decision = policy.evaluate_land(&ctx);
        let parent_open = if cr.depends_on.is_empty() {
            None
        } else {
            match self.record(cx, repo, &cr.depends_on).await {
                Ok((p, _)) if p.cr().state == ChangeState::Open as i32 => Some(p.cr().number),
                _ => None,
            }
        };
        Ok(Decision {
            decision,
            draft: cr.draft,
            parent_open,
        })
    }

    /// Whether the CR's stack applies cleanly on its target tip.
    async fn mergeable(&self, cx: &Cx, repo: &Repo, rec: &CrRecord) -> bool {
        let cr = rec.cr();
        let Ok(onto) = self.onto(cx, repo, &cr.target_ref, &cr.depends_on).await else {
            return false;
        };
        let base = cr.revisions.last().and_then(|r| r.base.clone());
        if base.as_ref() == Some(&onto) {
            return true;
        }
        matches!(
            self.vcs.rebase_stack(repo, &rec.stack, &onto).await,
            Ok(Rebased::Head(_))
        )
    }

    /// What policy says about landing the current revision for the caller right now.
    pub async fn evaluate(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<LandingEvaluation> {
        let (rec, _) = self.record(cx, repo, id).await?;
        let d = self
            .decide(cx, repo, &rec, &cx.chain, cx.actor_kind)
            .await?;
        let cr = rec.cr();
        let mut missing: Vec<String> = d.decision.missing.iter().map(|r| r.id.clone()).collect();
        // The reason LandChange would fail with now: state, draft, parent, then policy.
        let reason = if cr.state != ChangeState::Open as i32 || d.parent_open.is_some() {
            bee_proto::errors::reason_name(ErrorReason::InvalidState)
        } else if d.draft {
            bee_proto::errors::reason_name(ErrorReason::Draft)
        } else {
            d.decision
                .reason
                .map(|r| r.as_str())
                .unwrap_or(bee_proto::errors::reason_name(ErrorReason::PolicyDenied))
        };
        if cr.state != ChangeState::Open as i32 {
            missing.push("state:open".into());
        }
        if d.draft {
            missing.push("ready_for_review".into());
        }
        if let Some(n) = d.parent_open {
            missing.push(format!("parent:#{n}"));
        }
        let allowed = missing.is_empty() && d.decision.allowed;
        Ok(LandingEvaluation {
            rule: d.decision.rule.clone().unwrap_or_default(),
            reason: if allowed {
                String::new()
            } else {
                reason.to_string()
            },
            allowed,
            satisfied: d.decision.satisfied.iter().map(|r| r.id.clone()).collect(),
            missing,
            diff_class: cr
                .revisions
                .last()
                .map(|r| r.diff_class.clone())
                .unwrap_or_default(),
            revision: current_rev(cr),
            unresolved_thread_ids: self.unresolved_threads(cx, repo, id).await?,
            mergeable: self.mergeable(cx, repo, &rec).await,
        })
    }

    /// The land method to use, checked against the repo settings.
    async fn method(&self, cx: &Cx, repo: &Repo, m: LandMethod) -> Result<LandMethod> {
        let s = self.orgs.repo_settings(cx, &repo.id).await?;
        let m = if m == LandMethod::Unspecified {
            match method_of(s.default_land_method) {
                LandMethod::Unspecified => LandMethod::Rebase,
                d => d,
            }
        } else {
            m
        };
        if !s.allowed_land_methods.is_empty() && !s.allowed_land_methods.contains(&(m as i32)) {
            return Err(ForgeError::invalid(
                "land_method_not_allowed",
                format!("{} is not allowed in this repo", m.as_str_name()),
            ));
        }
        Ok(m)
    }

    /// Fails unless policy lets the chain land the CR now.
    #[allow(clippy::too_many_arguments)]
    async fn require_allowed(
        &self,
        cx: &Cx,
        repo: &Repo,
        rec: &CrRecord,
        chain: &[String],
        kind: PrincipalKind,
        ignore_checks: bool,
        checks_at: Option<&ObjectId>,
    ) -> Result<()> {
        let cr = rec.cr();
        if cr.draft {
            return Err(ForgeError::new(
                ErrorReason::Draft,
                format!("change request #{} is a draft", cr.number),
            ));
        }
        let d = Box::pin(self.decide_at(cx, repo, rec, chain, kind, checks_at)).await?;
        if let Some(n) = d.parent_open {
            return Err(ForgeError::invalid_state(format!(
                "the parent change request #{n} must land first"
            ))
            .with_subcode("parent_not_landed"));
        }
        let missing: Vec<String> = d
            .decision
            .missing
            .iter()
            .filter(|r| !(ignore_checks && r.kind == bee_policy::RequirementKind::Check))
            .map(|r| r.id.clone())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if d.decision.reason == Some(bee_policy::Reason::Leased) {
            let paths: Vec<String> = rec.paths.iter().map(|p| p.path.clone()).collect();
            let lander = chain.first().cloned().unwrap_or_default();
            self.leases
                .land_check(cx, &repo.id, &cr.target_ref, &paths, &lander)
                .await?;
        }
        Err(ForgeError::policy_denied(
            d.decision.rule.as_deref().unwrap_or("policy"),
            missing.clone(),
            format!("landing #{} needs {}", cr.number, missing.join(", ")),
        ))
    }

    fn default_message(rec: &CrRecord, method: LandMethod) -> String {
        let cr = rec.cr();
        match method {
            LandMethod::Squash => {
                if cr.body.trim().is_empty() {
                    format!("{} (#{})", cr.title, cr.number)
                } else {
                    format!("{} (#{})\n\n{}", cr.title, cr.number, cr.body)
                }
            }
            LandMethod::MergeCommit => format!("Land #{}: {}", cr.number, cr.title),
            _ => String::new(),
        }
    }

    // ----- landing ---------------------------------------------------------------------------

    /// Whether landing on `target_ref` must go through the merge queue: the repo setting
    /// `merge_queue_required`, or an active ruleset with a merge-queue rule that matches the
    /// ref ([`super::LandFacts::merge_queue_required`]). A failing ruleset lookup fails the
    /// caller rather than letting a land skip the queue.
    pub async fn queue_required(&self, cx: &Cx, repo: &Repo, target_ref: &str) -> Result<bool> {
        if self
            .orgs
            .repo_settings(cx, &repo.id)
            .await?
            .merge_queue_required
        {
            return Ok(true);
        }
        match self.land_facts() {
            Some(facts) => facts.merge_queue_required(repo, target_ref).await,
            None => Ok(false),
        }
    }

    /// Lands the CR's current revision now (policy first, then the [`super::Lander`]).
    pub async fn land(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        method: LandMethod,
        message: &str,
        expected_revision: u32,
    ) -> Result<(ChangeRequest, LandResponse)> {
        let (rec, _) = self.record(cx, repo, id).await?;
        if self.queue_required(cx, repo, &rec.cr().target_ref).await? {
            return Err(ForgeError::invalid_state(
                "this ref lands through the merge queue; enqueue the change request",
            )
            .with_subcode("merge_queue_required"));
        }
        self.land_inner(
            cx,
            repo,
            id,
            method,
            message,
            expected_revision,
            false,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn land_inner(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        method: LandMethod,
        message: &str,
        expected_revision: u32,
        via_queue: bool,
        combined: Option<&ObjectId>,
        auto_merge_expected: Option<AutoMerge>,
    ) -> Result<(ChangeRequest, LandResponse)> {
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        lock_change_request(&mut tx, &repo.id, id).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        rec.clear_expired_landing_intent(tx.now_ms())?;
        if let Some(expected) = auto_merge_expected.as_ref()
            && rec.cr().auto_merge.as_ref() != Some(expected)
        {
            return Err(ForgeError::invalid_state("auto-merge is no longer enabled"));
        }
        require_open(rec.cr())?;
        let rev = current_rev(rec.cr());
        if expected_revision != 0 && expected_revision != rev {
            return Err(ForgeError::version_mismatch(
                u64::from(expected_revision),
                u64::from(rev),
            ));
        }
        let landing_id = tx.new_id()?;
        rec.landing_started_at_ms = tx.now_ms();
        rec.landing_intent_id = landing_id.clone();
        rec.landing_head = head_of(rec.cr()).cloned();
        tx.update(&CRS, &row.clone().msg(&rec)).await?;
        tx.commit().await?;

        // The intent fences the head and reviews while policy dependencies read the store.
        // Run those reads after committing it, because SQLite transactions are immediate and
        // cannot nest another read transaction while this one is held.
        let method = match self.method(cx, repo, method).await {
            Ok(method) => method,
            Err(e) => {
                let _ = self.clear_landing_intent(cx, repo, id, &landing_id).await;
                return Err(e);
            }
        };
        if let Err(e) = self
            .require_allowed(cx, repo, &rec, &cx.chain, cx.actor_kind, false, combined)
            .await
        {
            let _ = self.clear_landing_intent(cx, repo, id, &landing_id).await;
            return Err(e);
        }
        let message = if message.is_empty() {
            Self::default_message(&rec, method)
        } else {
            message.to_string()
        };
        let cr = rec.cr();
        let target_ref = cr.target_ref.clone();
        let intent = LandIntent {
            ref_name: target_ref.clone(),
            // The merge queue lands exactly the combined result it tested.
            head: combined
                .cloned()
                .or_else(|| head_of(cr).cloned())
                .unwrap_or_default(),
            expected_version: 0,
            method,
            message,
            change_request_id: cr.id.clone(),
            chain: cx.chain.clone(),
            change_id: None,
        };
        let resp = match self.lander.land(repo, &intent).await {
            Ok(r) => r,
            Err(e) => {
                let _ = self.clear_landing_intent(cx, repo, id, &landing_id).await;
                if e.reason == ErrorReason::MergeConflict
                    && let Some(p) = e.payload.as_deref()
                    && let bee_proto::v1::error_detail::Payload::Conflicts(set) = p
                {
                    self.record_conflict(cx, repo, id, &target_ref, set.clone())
                        .await?;
                }
                return Err(e);
            }
        };
        let cr = match self
            .record_landed_for_intent(cx, repo, id, &resp, via_queue, &landing_id)
            .await
        {
            Ok(cr) => cr,
            Err(e) => {
                let _ = self.clear_landing_intent(cx, repo, id, &landing_id).await;
                return Err(e);
            }
        };
        Ok((cr, resp))
    }

    /// Records that a CR landed (`cr.landed`): state, landed ref, the pre-land tip for
    /// reverts, merge-queue bookkeeping, conflicts resolved, children retargeted onto the
    /// target ref. Called by [`Changes::land`] and by the server after a coalesced land.
    pub async fn record_landed(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        resp: &LandResponse,
        via_queue: bool,
    ) -> Result<ChangeRequest> {
        self.record_landed_inner(cx, repo, id, resp, via_queue, None)
            .await
    }

    async fn record_landed_for_intent(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        resp: &LandResponse,
        via_queue: bool,
        landing_id: &str,
    ) -> Result<ChangeRequest> {
        self.record_landed_inner(cx, repo, id, resp, via_queue, Some(landing_id))
            .await
    }

    async fn record_landed_inner(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        resp: &LandResponse,
        via_queue: bool,
        expected_landing_id: Option<&str>,
    ) -> Result<ChangeRequest> {
        let landed_ref = resp
            .r#ref
            .clone()
            .ok_or_else(|| ForgeError::missing("land.ref"))?;
        let target = landed_ref.target.clone().unwrap_or_default();
        let (rec, _) = self.record(cx, repo, id).await?;
        // The tip just before this CR's changes: a merge commit's first parent, else the
        // parent of the first landed result of the CR's own changes.
        let pre = {
            let t = self.vcs.commit(repo, &target).await?;
            if t.parents.len() > 1 {
                t.parents.first().cloned()
            } else {
                let own: BTreeSet<Vec<u8>> =
                    rec.cr().change_ids.iter().map(|c| c.raw.clone()).collect();
                match resp
                    .landed
                    .iter()
                    .find(|l| l.change_id.as_ref().is_some_and(|c| own.contains(&c.raw)))
                {
                    Some(l) => {
                        let r = l.result.clone().unwrap_or_default();
                        self.vcs.commit(repo, &r).await?.parents.first().cloned()
                    }
                    None => t.parents.first().cloned(),
                }
            }
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        lock_change_request(&mut tx, &repo.id, id).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        let now = tx.now_ms();
        if let Some(expected) = expected_landing_id {
            if rec.landing_intent_id != expected || rec.landing_head.as_ref() != head_of(rec.cr()) {
                return Err(ForgeError::invalid_state(
                    "the change request landing intent is no longer current",
                ));
            }
        } else {
            rec.clear_expired_landing_intent(now)?;
        }
        require_open(rec.cr())?;
        let before = cr_json(rec.cr());
        rec.clear_landing_intent();
        let target_ref = rec.cr().target_ref.clone();
        {
            let cr = rec.cr_mut();
            cr.state = ChangeState::Landed as i32;
            cr.landed_at = Some(ts(now));
            cr.landed_ref = Some(landed_ref.clone());
            cr.auto_merge = None;
        }
        rec.pre_land_tip = pre;
        // What this land put on the ref, for post-land bisect: the CR's own landed members and
        // the final target (a squash or merge result).
        let own: BTreeSet<Vec<u8>> = rec.cr().change_ids.iter().map(|c| c.raw.clone()).collect();
        let mut put: BTreeSet<String> = resp
            .landed
            .iter()
            .filter(|l| !l.skipped)
            .filter(|l| l.change_id.as_ref().is_some_and(|c| own.contains(&c.raw)))
            .filter_map(|l| l.result.as_ref().map(hex_of))
            .collect();
        if !target.raw.is_empty() {
            put.insert(hex_of(&target));
        }
        for cs in put {
            tx.put(
                &LANDED_BY,
                &Row::new(repo.id.clone(), cs).k1(id.to_string()),
            )
            .await?;
        }
        let queued = self
            .drop_queue_entry(&mut tx, repo, &mut rec, "landed")
            .await?;
        if queued && via_queue {
            let lid = tx.new_id()?;
            tx.insert(
                &QUEUE_LOG,
                &Row::new(format!("{}:{}", repo.id, target_ref), lid).n1(now as i64),
            )
            .await?;
        }
        let closes = rec.closing_issues();
        let (rec, _) = Self::save(&mut tx, rec, &row).await?;
        // Conflicts of this CR are resolved by the land.
        for (mut c, crow) in tx
            .scan_msgs::<LandConflict>(
                &CONFLICTS,
                &Scan::scope(repo.id.clone(), 1000).key(Key::K1(id.to_string())),
            )
            .await?
        {
            let st = LandConflictState::try_from(c.state).unwrap_or(LandConflictState::Open);
            if matches!(
                st,
                LandConflictState::Open | LandConflictState::ResolutionSubmitted
            ) {
                c.state = LandConflictState::Resolved as i32;
                tx.update(&CONFLICTS, &crow.clone().msg(&c)).await?;
            }
        }
        // Children now target the ref directly.
        let mut retargeted = Vec::new();
        for (mut child, crow) in tx
            .scan_msgs::<CrRecord>(
                &CRS,
                &Scan::scope(repo.id.clone(), super::store::MAX_EXAMINE),
            )
            .await?
        {
            if child.cr().depends_on != id || child.cr().state != ChangeState::Open as i32 {
                continue;
            }
            child.cr_mut().depends_on.clear();
            let (child, _) = Self::save(&mut tx, child, &crow).await?;
            Self::emit_cr(&mut tx, "cr.edited", &child, String::new()).await?;
            tx.audit(Self::audit_cr("cr.retarget", &child)).await?;
            retargeted.push(child.cr().id.clone());
        }
        if queued {
            Self::emit_cr(&mut tx, "merge_queue.left", &rec, String::new()).await?;
        }
        let payload = serde_json::json!({
            "closes_issues": closes,
            "retargeted": retargeted,
            "landed_ref": landed_ref.name,
            "landed_version": landed_ref.version,
        })
        .to_string();
        Self::emit_cr(&mut tx, "cr.landed", &rec, payload).await?;
        // The post publishes (or waits for a person's approval) with the land itself.
        crate::posts::store::on_landed(&mut tx, &rec).await?;
        tx.audit(Self::audit_cr("cr.land", &rec).before(&before))
            .await?;
        tx.commit().await?;
        let (rec, v) = self.record(cx, repo, id).await?;
        Ok(Self::served(&rec, v, &cx.actor_id))
    }

    /// Clears only the intent owned by a failed attempt, serialized with every other
    /// landing-sensitive change to this CR.
    async fn clear_landing_intent(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        landing_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        lock_change_request(&mut tx, &repo.id, id).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        if rec.landing_intent_id == landing_id {
            rec.clear_landing_intent();
            tx.update(&CRS, &row.clone().msg(&rec)).await?;
        }
        tx.commit().await
    }

    /// The CR whose land put `cs` on `ref_name` (any member of its stack, not only the final
    /// landed changeset). Reads the landed index; for lands recorded before it existed, falls
    /// back to the landed CRs of the ref: the first of `later` (the changesets after `cs` on
    /// the ref's first-parent chain, oldest first) that some CR landed as its final changeset
    /// names the CR, when that CR's change ids include `cs`'s.
    pub async fn landed_by(
        &self,
        cx: &Cx,
        repo: &Repo,
        ref_name: &str,
        cs: &ObjectId,
        later: &[ObjectId],
    ) -> Result<Option<ChangeRequest>> {
        let indexed = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.get(&LANDED_BY, &repo.id, &hex_of(cs)).await?
        };
        if let Some(row) = indexed
            && let Some(id) = row.k1.as_deref()
            && let Ok(cr) = self.get(cx, repo, id).await
            && cr.state == ChangeState::Landed as i32
        {
            return Ok(Some(cr));
        }
        let mut finals: std::collections::BTreeMap<Vec<u8>, ChangeRequest> =
            std::collections::BTreeMap::new();
        for (r, _) in self.records(cx, &repo.id).await? {
            let cr = r.cr();
            if cr.state != ChangeState::Landed as i32 || cr.target_ref != ref_name {
                continue;
            }
            if let Some(t) = cr.landed_ref.as_ref().and_then(|l| l.target.as_ref()) {
                finals.entry(t.raw.clone()).or_insert_with(|| cr.clone());
            }
        }
        if let Some(cr) = finals.get(&cs.raw) {
            return Ok(Some(cr.clone()));
        }
        let change_id = self.vcs.commit(repo, cs).await?.change_id;
        for c in later {
            if let Some(cr) = finals.get(&c.raw) {
                return Ok(cr
                    .change_ids
                    .iter()
                    .any(|x| x.raw == change_id.raw)
                    .then(|| cr.clone()));
            }
        }
        Ok(None)
    }

    // ----- auto-merge ------------------------------------------------------------------------

    /// Enables auto-merge: the CR lands as soon as policy allows (now, or on a later check
    /// or review event).
    pub async fn enable_auto_merge(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        method: LandMethod,
        message: &str,
    ) -> Result<ChangeRequest> {
        let s = self.orgs.repo_settings(cx, &repo.id).await?;
        if !s.allow_auto_merge {
            return Err(
                ForgeError::invalid_state("auto-merge is disabled for this repo")
                    .with_subcode("auto_merge_disabled"),
            );
        }
        let method = self.method(cx, repo, method).await?;
        let by = self.actor(cx).await;
        let chain = cx.chain.clone();
        let message = message.to_string();
        let now = self.db.now_ms();
        self.mutate_guarded(cx, repo, id, 0, move |rec, ev| {
            require_open(rec.cr())?;
            rec.land_chain = chain.clone();
            rec.cr_mut().auto_merge = Some(AutoMerge {
                enabled_by: Some(by),
                method: method as i32,
                commit_message: message,
                enabled_at: Some(ts(now)),
                on_behalf_of: chain,
            });
            ev.push(("cr.auto_merge_enabled", String::new()));
            Ok(Some("cr.enable_auto_merge".into()))
        })
        .await?;
        // Requirements may already hold.
        let _ = self.try_auto_merge(repo, id).await;
        self.get(cx, repo, id).await
    }

    /// Disables auto-merge.
    pub async fn disable_auto_merge(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
    ) -> Result<ChangeRequest> {
        self.mutate_guarded(cx, repo, id, 0, |rec, ev| {
            if rec.cr().auto_merge.is_none() {
                return Ok(None);
            }
            rec.cr_mut().auto_merge = None;
            ev.push(("cr.auto_merge_disabled", String::new()));
            Ok(Some("cr.disable_auto_merge".into()))
        })
        .await
    }

    /// The context of a stored land intent (auto-merge or merge queue): the chain that
    /// asked, with the first principal's kind from the directory.
    async fn intent_cx(&self, chain: &[String], fallback: &str, why: &str) -> Cx {
        let chain: Vec<String> = if chain.is_empty() {
            vec![fallback.to_string()]
        } else {
            chain.to_vec()
        };
        let actor = self.principal(&chain[0]).await;
        let kind = PrincipalKind::try_from(actor.kind).unwrap_or(PrincipalKind::Human);
        let mut cx = Cx::new(chain[0].clone(), kind).with_chain(chain);
        cx.decision = why.to_string();
        cx
    }

    /// Lands the CR if auto-merge is enabled and policy now allows it (enqueues it instead
    /// when the target ref requires the merge queue, by repo setting or ruleset). Returns the
    /// landed CR.
    pub async fn try_auto_merge(&self, repo: &Repo, id: &str) -> Result<Option<ChangeRequest>> {
        let sys = Cx::system();
        let (rec, _) = self.record(&sys, repo, id).await?;
        let cr = rec.cr();
        let Some(am) = cr.auto_merge.clone() else {
            return Ok(None);
        };
        if cr.state != ChangeState::Open as i32 || cr.draft {
            return Ok(None);
        }
        let by = am
            .enabled_by
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        let cx = self
            .intent_cx(&rec.land_chain, &by, &format!("auto-merge enabled by {by}"))
            .await;
        if self.queue_required(&cx, repo, &cr.target_ref).await? {
            if cr.merge_queue_entry.is_none()
                && self
                    .require_allowed(&cx, repo, &rec, &cx.chain, cx.actor_kind, true, None)
                    .await
                    .is_ok()
            {
                self.enqueue(&cx, repo, id, method_of(am.method), 0).await?;
            }
            return Ok(None);
        }
        if self
            .require_allowed(&cx, repo, &rec, &cx.chain, cx.actor_kind, false, None)
            .await
            .is_err()
        {
            return Ok(None);
        }
        let landed = self
            .land_inner(
                &cx,
                repo,
                id,
                method_of(am.method),
                &am.commit_message,
                0,
                false,
                None,
                Some(am.clone()),
            )
            .await;
        match landed {
            Ok((cr, _)) => Ok(Some(cr)),
            Err(e) if e.message == "auto-merge is no longer enabled" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// CRs with auto-merge enabled whose current head is `changeset` (hex).
    async fn auto_merge_for_head(&self, repo_id: &str, changeset: &str) -> Result<Vec<String>> {
        let mut tx = self.db.begin(SURFACE, &Cx::system()).await?;
        Ok(tx
            .scan_msgs::<CrRecord>(
                &CRS,
                &Scan::scope(repo_id, 1000).key(Key::K2(changeset.to_string())),
            )
            .await?
            .into_iter()
            .filter(|(r, _)| r.cr().auto_merge.is_some())
            .map(|(r, _)| r.cr().id.clone())
            .collect())
    }

    // ----- merge queue -----------------------------------------------------------------------

    /// Removes the CR's merge-queue entry inside `tx`; returns whether there was one.
    pub(crate) async fn drop_queue_entry(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        rec: &mut CrRecord,
        _reason: &str,
    ) -> Result<bool> {
        if rec.cr().merge_queue_entry.is_none() {
            return Ok(false);
        }
        let id = rec.cr().id.clone();
        tx.delete(&QUEUE, &repo.id, &id).await?;
        rec.cr_mut().merge_queue_entry = None;
        Ok(true)
    }

    /// Adds the CR to its target ref's merge queue. Approvals and every other requirement
    /// except checks must hold (checks run on the queued result). A stacked CR enters after
    /// its parent. Idempotent for a CR already queued.
    pub async fn enqueue(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        method: LandMethod,
        priority: u32,
    ) -> Result<MergeQueueEntry> {
        let (rec, _) = self.record(cx, repo, id).await?;
        require_open(rec.cr())?;
        if let Some(e) = &rec.cr().merge_queue_entry {
            return Ok(e.clone());
        }
        let queue_limit = if let Some(facts) = self.land_facts()
            && let Some(params) = facts.merge_queue_params(repo, &rec.cr().target_ref).await?
        {
            params.max_entries
        } else {
            0
        };
        let method = self.method(cx, repo, method).await?;
        if !rec.cr().depends_on.is_empty() {
            let (parent, _) = self.record(cx, repo, &rec.cr().depends_on).await?;
            let p = parent.cr();
            if p.state == ChangeState::Open as i32 && p.merge_queue_entry.is_none() {
                return Err(ForgeError::invalid_state(format!(
                    "enqueue the parent change request #{} first",
                    p.number
                ))
                .with_subcode("parent_not_queued"));
            }
        }
        // A queued parent lands first, so its being open is not a blocker here.
        let d = self
            .decide(cx, repo, &rec, &cx.chain, cx.actor_kind)
            .await?;
        if rec.cr().draft {
            return Err(ForgeError::new(
                ErrorReason::Draft,
                format!("change request #{} is a draft", rec.cr().number),
            ));
        }
        let missing: Vec<String> = d
            .decision
            .missing
            .iter()
            .filter(|r| r.kind != bee_policy::RequirementKind::Check)
            .map(|r| r.id.clone())
            .collect();
        if !missing.is_empty() {
            return Err(ForgeError::policy_denied(
                d.decision.rule.as_deref().unwrap_or("policy"),
                missing.clone(),
                format!("queueing needs {}", missing.join(", ")),
            ));
        }
        let by = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        if queue_limit != 0 {
            // The counter row is a per-ref lock on every store backend. Hold it while
            // counting and inserting so concurrent enqueues cannot both take the last slot.
            let capacity_lock = format!("queue_capacity:{}", rec.cr().target_ref);
            tx.add_n1(&super::COUNTERS, &repo.id, &capacity_lock, 0)
                .await?;
            let entries = tx
                .scan_msgs::<MergeQueueEntry>(
                    &QUEUE,
                    &Scan::scope(repo.id.clone(), 10_000)
                        .key(Key::K1(rec.cr().target_ref.clone()))
                        .by_n1(),
                )
                .await?;
            if entries.len() >= queue_limit as usize {
                return Err(ForgeError::new(
                    ErrorReason::ResourceExhausted,
                    format!(
                        "the merge queue for {} has reached its {} entry limit",
                        rec.cr().target_ref,
                        queue_limit
                    ),
                )
                .with_subcode("merge_queue_full"));
            }
        }
        let before = cr_json(rec.cr());
        let now = tx.now_ms();
        let seq = tx
            .add_n1(&super::COUNTERS, &repo.id, "queue_seq", 1)
            .await?;
        let entry = MergeQueueEntry {
            change_request_id: id.to_string(),
            change_number: rec.cr().number,
            r#ref: rec.cr().target_ref.clone(),
            position: 0,
            state: MergeQueueState::Queued as i32,
            enqueued_by: Some(by),
            enqueued_at: Some(ts(now)),
            estimated_land_at: None,
            priority,
            eject_reason: String::new(),
            speculative_head: None,
            title: rec.cr().title.clone(),
            land_method: method as i32,
            on_behalf_of: cx.chain.clone(),
        };
        tx.insert(
            &QUEUE,
            &Row::new(repo.id.clone(), id.to_string())
                .k1(entry.r#ref.clone())
                .n1(order_key(priority, seq))
                .msg(&entry),
        )
        .await?;
        rec.cr_mut().merge_queue_entry = Some(entry.clone());
        rec.queue_method = method as i32;
        rec.land_chain = cx.chain.clone();
        let (rec, _) = Self::save(&mut tx, rec, &row).await?;
        Self::emit_cr(&mut tx, "merge_queue.entered", &rec, String::new()).await?;
        tx.audit(
            AuditDraft::new("merge_queue.enqueue", &rec.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::ChangeRequest, id)
                .before(&before)
                .after(&serde_json::to_string(&entry).unwrap_or_default()),
        )
        .await?;
        tx.commit().await?;
        let (entries, _) = self.queue(cx, repo, &entry.r#ref).await?;
        Ok(entries
            .into_iter()
            .find(|e| e.change_request_id == id)
            .unwrap_or(entry))
    }

    /// Removes the CR from the merge queue.
    pub async fn dequeue(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        if rec.cr().merge_queue_entry.is_none() {
            return Err(
                ForgeError::invalid_state("the change request is not queued")
                    .with_subcode("not_queued"),
            );
        }
        let before = cr_json(rec.cr());
        self.drop_queue_entry(&mut tx, repo, &mut rec, "dequeued")
            .await?;
        let (rec, _) = Self::save(&mut tx, rec, &row).await?;
        Self::emit_cr(&mut tx, "merge_queue.left", &rec, String::new()).await?;
        tx.audit(
            AuditDraft::new("merge_queue.dequeue", &rec.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::ChangeRequest, id)
                .before(&before),
        )
        .await?;
        tx.commit().await
    }

    /// The merge queue of a ref in landing order with positions (1 = next) and estimated
    /// land times, and the recent throughput in lands per minute.
    pub async fn queue(
        &self,
        cx: &Cx,
        repo: &Repo,
        ref_name: &str,
    ) -> Result<(Vec<MergeQueueEntry>, u32)> {
        let ref_name = if ref_name.is_empty() {
            repo.default_ref.as_str()
        } else {
            ref_name
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let rows = tx
            .scan_msgs::<MergeQueueEntry>(
                &QUEUE,
                &Scan::scope(repo.id.clone(), 10_000)
                    .key(Key::K1(ref_name.to_string()))
                    .by_n1(),
            )
            .await?;
        let landed = tx
            .count_window(&format!("{}:{ref_name}", repo.id), now)
            .await?;
        let per_minute = landed.div_ceil(THROUGHPUT_WINDOW_MS / 60_000) as u32;
        let interval_ms = if per_minute == 0 {
            60_000
        } else {
            60_000 / u64::from(per_minute)
        };
        let entries = rows
            .into_iter()
            .enumerate()
            .map(|(i, (mut e, _))| {
                e.position = i as u32 + 1;
                e.estimated_land_at = Some(ts(now + interval_ms * (i as u64 + 1)));
                e
            })
            .collect();
        Ok((entries, per_minute))
    }

    /// Updates a queued CR's state as the coalescer reports it. `EJECTED` removes it from
    /// the queue (`merge_queue.left`, with `eject_reason`); `LANDED` is recorded by
    /// [`Changes::record_landed`].
    pub async fn set_queue_state(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        state: MergeQueueState,
        speculative_head: Option<ObjectId>,
        eject_reason: &str,
    ) -> Result<Option<MergeQueueEntry>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        let Some(mut entry) = rec.cr().merge_queue_entry.clone() else {
            return Err(
                ForgeError::invalid_state("the change request is not queued")
                    .with_subcode("not_queued"),
            );
        };
        let before = cr_json(rec.cr());
        let out = if state == MergeQueueState::Ejected {
            entry.state = state as i32;
            entry.eject_reason = eject_reason.to_string();
            self.drop_queue_entry(&mut tx, repo, &mut rec, eject_reason)
                .await?;
            let (rec2, _) = Self::save(&mut tx, rec, &row).await?;
            rec = rec2;
            Self::emit_cr(
                &mut tx,
                "merge_queue.left",
                &rec,
                serde_json::json!({ "eject_reason": eject_reason }).to_string(),
            )
            .await?;
            None
        } else {
            entry.state = state as i32;
            if speculative_head.is_some() {
                entry.speculative_head = speculative_head;
            }
            let qrow = tx
                .get(&QUEUE, &repo.id, id)
                .await?
                .ok_or_else(|| ForgeError::not_found("merge-queue entry"))?;
            tx.update(&QUEUE, &qrow.msg(&entry)).await?;
            rec.cr_mut().merge_queue_entry = Some(entry.clone());
            let (rec2, _) = Self::save(&mut tx, rec, &row).await?;
            rec = rec2;
            Self::emit_cr(&mut tx, "merge_queue.updated", &rec, String::new()).await?;
            Some(entry.clone())
        };
        tx.audit(
            AuditDraft::new("merge_queue.update", &rec.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::ChangeRequest, id)
                .before(&before)
                .after(&serde_json::to_string(&entry).unwrap_or_default()),
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Lands a queued CR for the principal that enqueued it (the coalescer's land step).
    /// A refused land ejects the entry with the error's reason.
    pub async fn land_queued(
        &self,
        repo: &Repo,
        id: &str,
    ) -> Result<(ChangeRequest, LandResponse)> {
        self.land_queued_at(repo, id, None).await
    }

    /// The context a queued CR lands (and is evaluated) for: the chain that enqueued it.
    async fn queue_cx(&self, rec: &CrRecord) -> Result<Cx> {
        let entry = rec
            .cr()
            .merge_queue_entry
            .clone()
            .ok_or_else(|| ForgeError::invalid_state("the change request is not queued"))?;
        let by = entry
            .enqueued_by
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        Ok(self
            .intent_cx(
                &rec.land_chain,
                &by,
                &format!("merge queue, enqueued by {by}"),
            )
            .await)
    }

    /// [`Changes::land_queued`], landing the tested combined result `combined` when given.
    async fn land_queued_at(
        &self,
        repo: &Repo,
        id: &str,
        combined: Option<&ObjectId>,
    ) -> Result<(ChangeRequest, LandResponse)> {
        let sys = Cx::system();
        let (rec, _) = self.record(&sys, repo, id).await?;
        let entry = rec
            .cr()
            .merge_queue_entry
            .clone()
            .ok_or_else(|| ForgeError::invalid_state("the change request is not queued"))?;
        let by = entry
            .enqueued_by
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        let cx = self
            .intent_cx(
                &rec.land_chain,
                &by,
                &format!("merge queue, enqueued by {by}"),
            )
            .await;
        self.set_queue_state(&cx, repo, id, MergeQueueState::Landing, None, "")
            .await?;
        match self
            .land_inner(
                &cx,
                repo,
                id,
                method_of(rec.queue_method),
                "",
                0,
                true,
                combined,
                None,
            )
            .await
        {
            Ok(r) => Ok(r),
            Err(e) => {
                let _ = self
                    .set_queue_state(
                        &cx,
                        repo,
                        id,
                        MergeQueueState::Ejected,
                        None,
                        e.reason_name(),
                    )
                    .await;
                Err(e)
            }
        }
    }

    /// One step of the merge queue of `ref_name` (what the server's queue worker runs):
    ///
    /// - By default the head entry waits while only checks are missing (on the CR's own head)
    ///   and is landed otherwise ([`Changes::land_queued`], which ejects a refused land).
    /// - With the repo setting `merge_queue_check_combined` the queue tests what would land:
    ///   the head entry's stack is rebased onto the target tip (a conflict ejects it with
    ///   `MERGE_CONFLICT`), the entry goes to `CHECKING` with that combined result as its
    ///   `speculative_head`, and `merge_queue.checks_requested` (the `merge_group` workflow
    ///   event) asks for checks on it. The required checks are then read from the combined
    ///   result: a failing one ejects the entry (`POLICY_DENIED`, with a comment on the CR
    ///   naming the checks), pending ones keep it waiting,
    ///   and when they pass the combined result itself lands. A result built on an older tip is
    ///   rebuilt first. Requirements other than checks are evaluated as for enqueueing; a
    ///   missing one ejects the entry (`POLICY_DENIED`).
    ///
    /// Entries are tested one at a time, in queue order, each on the tip its predecessor
    /// left, so what lands is always exactly what was tested.
    pub async fn step_queue(&self, cx: &Cx, repo: &Repo, ref_name: &str) -> Result<QueueStep> {
        let (entries, _) = self.queue(cx, repo, ref_name).await?;
        let Some(first) = entries.into_iter().next() else {
            return Ok(QueueStep::Empty);
        };
        let id = first.change_request_id.clone();
        let (rec, _) = self.record(cx, repo, &id).await?;
        let qcx = self.queue_cx(&rec).await?;
        let combined = self
            .orgs
            .repo_settings(cx, &repo.id)
            .await?
            .merge_queue_check_combined;
        if !combined {
            if let Ok(ev) = Box::pin(self.evaluate(&qcx, repo, &id)).await
                && !ev.allowed
                && !ev.missing.is_empty()
                && ev.missing.iter().all(|m| m.starts_with("check:"))
            {
                return Ok(QueueStep::Waiting);
            }
            return Box::pin(self.finish_step(repo, &id, None)).await;
        }
        let cr = rec.cr();
        let tip = self.onto(cx, repo, &cr.target_ref, "").await?;
        let state = cr
            .merge_queue_entry
            .as_ref()
            .map(|e| MergeQueueState::try_from(e.state).unwrap_or(MergeQueueState::Queued))
            .unwrap_or(MergeQueueState::Queued);
        let spec = cr
            .merge_queue_entry
            .as_ref()
            .and_then(|e| e.speculative_head.clone())
            .filter(|s| !s.raw.is_empty());
        let d = match &spec {
            Some(s)
                if state == MergeQueueState::Checking && rec.queue_base.as_ref() == Some(&tip) =>
            {
                Box::pin(self.decide_at(&qcx, repo, &rec, &qcx.chain, qcx.actor_kind, Some(s)))
                    .await?
            }
            _ => {
                // Build the combined result on the current tip and ask for its checks.
                let d = Box::pin(self.decide(&qcx, repo, &rec, &qcx.chain, qcx.actor_kind)).await?;
                if let Some(why) = Self::queue_blocker(&rec, &d) {
                    return Box::pin(self.eject(&qcx, repo, &id, ErrorReason::PolicyDenied, &why))
                        .await;
                }
                return match self.vcs.rebase_stack(repo, &rec.stack, &tip).await? {
                    Rebased::Conflict(set) => {
                        let _ = self
                            .record_conflict(&qcx, repo, &id, &cr.target_ref, set)
                            .await;
                        Box::pin(self.eject(
                            &qcx,
                            repo,
                            &id,
                            ErrorReason::MergeConflict,
                            "the stack does not apply on the target tip",
                        ))
                        .await
                    }
                    Rebased::Head(s) => {
                        Box::pin(self.start_checking(&qcx, repo, &id, &s, &tip)).await?;
                        Ok(QueueStep::Waiting)
                    }
                };
            }
        };
        let Some(s) = spec else {
            return Ok(QueueStep::Waiting);
        };
        if let Some(why) = Self::queue_blocker(&rec, &d) {
            return Box::pin(self.eject(&qcx, repo, &id, ErrorReason::PolicyDenied, &why)).await;
        }
        let missing: Vec<String> = d
            .decision
            .missing
            .iter()
            .filter(|r| r.kind == bee_policy::RequirementKind::Check)
            .map(|r| r.id.trim_start_matches("check:").to_string())
            .collect();
        if !missing.is_empty() {
            let results = self.checks.check_results(cx, &repo.id, &hex_of(&s)).await?;
            let mut failed: Vec<String> = results
                .iter()
                .filter(|c| missing.contains(&c.name))
                .filter(|c| c.status == bee_policy::CheckStatus::Failure)
                .map(|c| c.name.clone())
                .collect();
            if failed.is_empty() {
                return Ok(QueueStep::Waiting);
            }
            failed.sort();
            failed.dedup();
            let why = format!(
                "required checks failed on the combined result {}: {}",
                hex_of(&s),
                failed.join(", ")
            );
            return Box::pin(self.eject(&qcx, repo, &id, ErrorReason::PolicyDenied, &why)).await;
        }
        Box::pin(self.finish_step(repo, &id, Some(&s))).await
    }

    /// What, other than checks, keeps a queued CR from landing.
    fn queue_blocker(rec: &CrRecord, d: &Decision) -> Option<String> {
        if rec.cr().draft {
            return Some(format!("change request #{} is a draft", rec.cr().number));
        }
        let missing: Vec<String> = d
            .decision
            .missing
            .iter()
            .filter(|r| r.kind != bee_policy::RequirementKind::Check)
            .map(|r| r.id.clone())
            .collect();
        (!missing.is_empty()).then(|| format!("landing needs {}", missing.join(", ")))
    }

    async fn finish_step(
        &self,
        repo: &Repo,
        id: &str,
        combined: Option<&ObjectId>,
    ) -> Result<QueueStep> {
        match Box::pin(self.land_queued_at(repo, id, combined)).await {
            Ok((cr, _)) => Ok(QueueStep::Landed(Box::new(cr))),
            // The entry was ejected with the reason.
            Err(e) if !e.retryable() => Ok(QueueStep::Ejected(
                id.to_string(),
                e.reason_name().to_string(),
            )),
            Err(e) => Err(e),
        }
    }

    async fn eject(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        reason: ErrorReason,
        why: &str,
    ) -> Result<QueueStep> {
        let name = bee_proto::errors::reason_name(reason);
        self.set_queue_state(cx, repo, id, MergeQueueState::Ejected, None, name)
            .await?;
        let (rec, _) = self.record(cx, repo, id).await?;
        let body = format!("Removed from the merge queue ({name}): {why}");
        let _ = self
            .comment(&Cx::system(), repo, &rec.cr().id, &body, false)
            .await;
        Ok(QueueStep::Ejected(id.to_string(), name.to_string()))
    }

    /// Puts the head entry in `CHECKING` on its combined result `head` (built on `base`) and
    /// requests checks on it (`merge_queue.checks_requested`, the `merge_group` event).
    async fn start_checking(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        head: &ObjectId,
        base: &ObjectId,
    ) -> Result<()> {
        let message = self
            .vcs
            .commit(repo, head)
            .await
            .map(|c| c.message)
            .unwrap_or_default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        let Some(mut entry) = rec.cr().merge_queue_entry.clone() else {
            return Err(
                ForgeError::invalid_state("the change request is not queued")
                    .with_subcode("not_queued"),
            );
        };
        let before = cr_json(rec.cr());
        entry.state = MergeQueueState::Checking as i32;
        entry.speculative_head = Some(head.clone());
        let qrow = tx
            .get(&QUEUE, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found("merge-queue entry"))?;
        tx.update(&QUEUE, &qrow.msg(&entry)).await?;
        rec.cr_mut().merge_queue_entry = Some(entry.clone());
        rec.queue_base = Some(base.clone());
        let number = rec.cr().number;
        let target = rec.cr().target_ref.clone();
        let (rec, _) = Self::save(&mut tx, rec, &row).await?;
        Self::emit_cr(&mut tx, "merge_queue.updated", &rec, String::new()).await?;
        let payload = serde_json::json!({
            "base_ref": target,
            "base_sha": hex_of(base),
            "head_sha": hex_of(head),
            "head_ref": format!("bee-queue/{target}/cr-{number}-{}", hex_of(head)),
            "head_commit_message": message,
        })
        .to_string();
        Self::emit_cr(&mut tx, "merge_queue.checks_requested", &rec, payload).await?;
        tx.audit(
            AuditDraft::new("merge_queue.check", &rec.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::ChangeRequest, id)
                .before(&before)
                .after(&serde_json::to_string(&entry).unwrap_or_default()),
        )
        .await?;
        tx.commit().await
    }

    /// One step of a reference coalescer: lands the first entry of `ref_name`'s queue.
    /// Returns `None` when the queue is empty.
    pub async fn process_queue(
        &self,
        cx: &Cx,
        repo: &Repo,
        ref_name: &str,
    ) -> Result<Option<(ChangeRequest, LandResponse)>> {
        let (entries, _) = self.queue(cx, repo, ref_name).await?;
        let Some(first) = entries.first() else {
            return Ok(None);
        };
        self.land_queued(repo, &first.change_request_id)
            .await
            .map(Some)
    }

    // ----- land conflicts ---------------------------------------------------------------------

    /// Holds a conflict for a resolver (`land_conflict.opened`).
    pub async fn record_conflict(
        &self,
        cx: &Cx,
        repo: &Repo,
        cr_id: &str,
        ref_name: &str,
        conflicts: bee_proto::v1::ConflictSet,
    ) -> Result<LandConflict> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, cr_id).await?;
        let now = tx.now_ms();
        let cid = tx.new_id()?;
        let c = LandConflict {
            id: cid.clone(),
            repo_id: repo.id.clone(),
            change_request_id: cr_id.to_string(),
            r#ref: ref_name.to_string(),
            conflicts: Some(conflicts),
            state: LandConflictState::Open as i32,
            resolution: None,
            resolver: None,
            created_at: Some(ts(now)),
            error_reason: String::new(),
        };
        tx.insert(
            &CONFLICTS,
            &Row::new(repo.id.clone(), cid.clone())
                .k1(cr_id.to_string())
                .n1(now as i64)
                .msg(&c),
        )
        .await?;
        Self::emit_cr(
            &mut tx,
            "land_conflict.opened",
            &rec,
            serde_json::to_string(&c).unwrap_or_default(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("land_conflict.open", &rec.org_id)
                .repo_id(&repo.id)
                .target_id(&cid),
        )
        .await?;
        tx.commit().await?;
        Ok(c)
    }

    /// A land conflict.
    pub async fn conflict(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<LandConflict> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.get_msg::<LandConflict>(&CONFLICTS, &repo.id, id)
            .await?
            .map(|(c, _)| c)
            .ok_or_else(|| ForgeError::not_found(format!("land conflict {id}")))
    }

    /// Land conflicts of a repo, newest first, optionally in one state; paged.
    pub async fn conflicts(
        &self,
        cx: &Cx,
        repo: &Repo,
        state: LandConflictState,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<LandConflict>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan_msgs::<LandConflict>(
                &CONFLICTS,
                &Scan::scope(repo.id.clone(), 10_000)
                    .desc()
                    .after((!token.is_empty()).then(|| Cursor {
                        n1: 0,
                        scope: repo.id.clone(),
                        id: token.to_string(),
                    })),
            )
            .await?;
        let mut out = Vec::new();
        let mut next = String::new();
        for (c, row) in rows {
            if state != LandConflictState::Unspecified && c.state != state as i32 {
                continue;
            }
            if out.len() == limit {
                break;
            }
            next = row.id.clone();
            out.push(c);
        }
        if out.len() < limit {
            next.clear();
        }
        Ok((out, next))
    }

    /// Submits a resolution changeset: it becomes the CR's new revision (and goes through
    /// the same checks and policy as any change); with `land`, the CR lands once policy
    /// allows (auto-merge).
    pub async fn submit_resolution(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        resolution: &ObjectId,
        land: bool,
    ) -> Result<LandConflict> {
        if resolution.raw.is_empty() {
            return Err(ForgeError::missing("resolution"));
        }
        let c = self.conflict(cx, repo, id).await?;
        let st = LandConflictState::try_from(c.state).unwrap_or(LandConflictState::Open);
        if !matches!(
            st,
            LandConflictState::Open | LandConflictState::ResolutionSubmitted
        ) {
            return Err(ForgeError::invalid_state("the conflict is closed"));
        }
        let info = self.vcs.commit(repo, resolution).await?;
        self.revise(cx, repo, &c.change_request_id, resolution)
            .await?;
        let resolver = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, &c.change_request_id).await?;
        let (mut c, row) = tx
            .get_msg::<LandConflict>(&CONFLICTS, &repo.id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("land conflict {id}")))?;
        c.state = LandConflictState::ResolutionSubmitted as i32;
        c.resolution = Some(resolution.clone());
        c.resolver = Some(resolver);
        c.error_reason.clear();
        tx.update(&CONFLICTS, &row.clone().msg(&c)).await?;
        Self::emit_cr(
            &mut tx,
            "land_conflict.resolved",
            &rec,
            serde_json::to_string(&c).unwrap_or_default(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("land_conflict.resolve", &rec.org_id)
                .repo_id(&repo.id)
                .target_id(id),
        )
        .await?;
        tx.commit().await?;
        if land {
            let method = if info.parents.len() > 1 {
                LandMethod::MergeCommit
            } else {
                LandMethod::Rebase
            };
            if let Err(e) = self
                .enable_auto_merge(cx, repo, &c.change_request_id, method, "")
                .await
            {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                if let Some((mut c2, row)) =
                    tx.get_msg::<LandConflict>(&CONFLICTS, &repo.id, id).await?
                {
                    c2.error_reason = e.reason_name().to_string();
                    tx.update(&CONFLICTS, &row.clone().msg(&c2)).await?;
                    tx.commit().await?;
                }
            }
        }
        self.conflict(cx, repo, id).await
    }

    // ----- revert and update base -------------------------------------------------------------

    /// Opens a CR that reverts a landed CR, on the current tip of its target ref.
    pub async fn revert(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<ChangeRequest> {
        let (rec, _) = self.record(cx, repo, id).await?;
        let cr = rec.cr();
        if cr.state != ChangeState::Landed as i32 {
            return Err(ForgeError::invalid_state(format!(
                "change request #{} has not landed",
                cr.number
            )));
        }
        let landed = cr
            .landed_ref
            .as_ref()
            .and_then(|r| r.target.clone())
            .ok_or_else(|| ForgeError::invalid_state("the landed ref is unknown"))?;
        let restore = rec
            .pre_land_tip
            .clone()
            .ok_or_else(|| ForgeError::invalid_state("the pre-land tip is unknown"))?;
        let tip = self.onto(cx, repo, &cr.target_ref, "").await?;
        let title = if title.trim().is_empty() {
            super::text::revert_title(&cr.title)
        } else {
            title.trim().to_string()
        };
        let actor = self.actor(cx).await;
        let head = match self
            .vcs
            .revert(
                repo,
                &restore,
                &landed,
                &tip,
                NewCommit {
                    parent: None,
                    edits: Vec::new(),
                    message: format!("{title}\n\nThis reverts change request #{}.", cr.number),
                    author: if actor.display.is_empty() {
                        actor.id.clone()
                    } else {
                        actor.display.clone()
                    },
                    creator: cx.actor_id.clone(),
                    chain: cx.chain.clone(),
                    change_id: None,
                },
            )
            .await?
        {
            Rebased::Head(h) => h,
            Rebased::Conflict(set) => return Err(merge_conflict(set)),
        };
        self.open(
            cx,
            repo,
            NewChange {
                target_ref: cr.target_ref.clone(),
                head: Some(head),
                title,
                body: if body.is_empty() {
                    format!("Reverts #{}", cr.number)
                } else {
                    body.to_string()
                },
                draft,
                revert_of: cr.id.clone(),
                ..NewChange::default()
            },
        )
        .await
    }

    /// Brings the CR up to date with its target ("update branch"): rebases its stack onto
    /// the current tip (or the parent CR's head), or merges the tip in with `MERGE_COMMIT`.
    pub async fn update_base(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        method: LandMethod,
    ) -> Result<ChangeRequest> {
        let (rec, _) = self.record(cx, repo, id).await?;
        require_open(rec.cr())?;
        let cr = rec.cr();
        let onto = self.onto(cx, repo, &cr.target_ref, &cr.depends_on).await?;
        let base = cr.revisions.last().and_then(|r| r.base.clone());
        if base.as_ref() == Some(&onto) {
            return self.get(cx, repo, id).await;
        }
        let head = head_of(cr).cloned().unwrap_or_default();
        let out = match method {
            LandMethod::MergeCommit => {
                let actor = self.actor(cx).await;
                self.vcs
                    .merge(
                        repo,
                        &head,
                        &onto,
                        NewCommit {
                            message: format!("Merge {} into #{}", cr.target_ref, cr.number),
                            author: actor.id.clone(),
                            creator: cx.actor_id.clone(),
                            chain: cx.chain.clone(),
                            ..NewCommit::default()
                        },
                    )
                    .await?
            }
            LandMethod::Squash => {
                return Err(ForgeError::invalid(
                    "update_base_method",
                    "update base takes REBASE or MERGE_COMMIT",
                ));
            }
            _ => self.vcs.rebase_stack(repo, &rec.stack, &onto).await?,
        };
        match out {
            Rebased::Head(h) => self.revise(cx, repo, id, &h).await,
            Rebased::Conflict(set) => {
                self.record_conflict(cx, repo, id, &cr.target_ref, set.clone())
                    .await?;
                Err(merge_conflict(set))
            }
        }
    }

    // ----- repo context ------------------------------------------------------------------------

    /// Everything an agent should know before editing `paths` (SPEC §11): owners, live
    /// leases, open CRs touching the paths, failing checks at `at`, the land rules of the
    /// default ref, agent instruction files, the policy epoch and which paths are sensitive.
    pub async fn context(
        &self,
        cx: &Cx,
        repo: &Repo,
        at: &str,
        paths: &[Vec<u8>],
    ) -> Result<RepoContext> {
        let want = crate::service::utf8_paths(paths)?;
        let at_id = if at.is_empty() {
            self.vcs
                .ref_tip(repo, &repo.default_ref)
                .await?
                .and_then(|r| r.target)
        } else {
            match self.vcs.ref_tip(repo, at).await? {
                Some(Ref {
                    target: Some(t), ..
                }) => Some(t),
                _ => Some(self.resolver.resolve(repo, at).await?),
            }
        };
        let policy = self.effective_policy(repo).await?;
        let owners = want
            .iter()
            .map(|p| PathOwners {
                path: p.as_bytes().to_vec(),
                owners: policy
                    .owners_of(p)
                    .map(|(_, o)| o.iter().map(|x| x.to_string()).collect())
                    .unwrap_or_default(),
            })
            .collect();
        let active_leases = self.leases.list(cx, &repo.id, paths).await?;
        let touching = |rec: &CrRecord| {
            want.is_empty()
                || rec.paths.iter().any(|p| {
                    want.iter().any(|w| {
                        crate::leases::covers(w, &p.path) || crate::leases::covers(&p.path, w)
                    })
                })
        };
        let open_changes_touching = self
            .records(cx, &repo.id)
            .await?
            .into_iter()
            .filter(|(r, _)| r.cr().state == ChangeState::Open as i32 && touching(r))
            .take(super::store::MAX_ITEMS)
            .map(|(r, v)| Self::served(&r, v, &cx.actor_id))
            .collect();
        let failing_checks = match &at_id {
            Some(id) => self
                .checks
                .list(
                    cx,
                    &repo.id,
                    &crate::checks::RunFilter {
                        changeset_hex: Some(hex_of(id)),
                        latest_only: true,
                        ..Default::default()
                    },
                )
                .await?
                .into_iter()
                .filter(|r| {
                    crate::checks::policy_status(
                        CheckStatus::try_from(r.status).unwrap_or(CheckStatus::Unspecified),
                    ) == bee_policy::CheckStatus::Failure
                })
                .collect(),
            None => Vec::new(),
        };
        let land_rules = land_summary(&policy, &repo.default_ref);
        let mut instructions = Vec::new();
        if let Some(id) = &at_id {
            let mut dirs: BTreeSet<String> = BTreeSet::from([String::new()]);
            for p in &want {
                let mut acc = String::new();
                for comp in p.split('/').filter(|c| !c.is_empty()) {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(comp);
                    dirs.insert(acc.clone());
                }
            }
            let mut files: Vec<String> = Vec::new();
            for d in &dirs {
                for f in super::text::INSTRUCTION_FILES {
                    files.push(if d.is_empty() {
                        f.to_string()
                    } else {
                        format!("{d}/{f}")
                    });
                }
            }
            for n in self.vcs.list_dir(repo, id, ".github/agents").await? {
                if n.ends_with(".md") {
                    files.push(format!(".github/agents/{n}"));
                }
            }
            for f in files {
                if let Some(bytes) = self.vcs.read_file(repo, id, &f).await? {
                    let truncated = bytes.len() > MAX_INSTRUCTION_BYTES;
                    let mut end = bytes.len().min(MAX_INSTRUCTION_BYTES);
                    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
                        end -= 1;
                    }
                    instructions.push(ContextInstruction {
                        path: f.as_bytes().to_vec(),
                        blob: self.vcs.file_id(repo, id, &f).await?,
                        content: String::from_utf8_lossy(&bytes[..end]).into_owned(),
                        truncated,
                    });
                }
            }
        }
        Ok(RepoContext {
            owners,
            active_leases,
            open_changes_touching,
            failing_checks,
            land_rules,
            instructions,
            policy_epoch: policy.epoch().to_string(),
            sensitive_paths: want
                .iter()
                .filter(|p| policy.is_sensitive(p))
                .cloned()
                .collect(),
        })
    }
}

/// Human-readable lines for the land rule of a ref.
pub fn land_summary(policy: &bee_policy::Policy, ref_name: &str) -> Vec<String> {
    let Some(b) = land_rule(policy, ref_name) else {
        return vec![format!("{ref_name}: no land rule; write access lands")];
    };
    let mut out = Vec::new();
    if !b.required_checks.is_empty() {
        let v: Vec<&str> = b.required_checks.iter().map(String::as_str).collect();
        out.push(format!("required checks: {}", v.join(", ")));
    }
    for a in &b.approvals {
        let classes: Vec<&str> = a.classes.iter().map(|c| c.as_str()).collect();
        let paths: Vec<&str> = a.paths.iter().map(String::as_str).collect();
        let from = match &a.from {
            bee_policy::ApproverSet::Any => "anyone with review".to_string(),
            bee_policy::ApproverSet::Owners => "owners".to_string(),
            bee_policy::ApproverSet::Subjects(s) => s
                .iter()
                .map(|x| format!("{x:?}"))
                .collect::<Vec<_>>()
                .join(", "),
        };
        out.push(format!(
            "{} approval(s) from {from}{}{}{}",
            a.count,
            if classes.is_empty() {
                String::new()
            } else {
                format!(" for {}", classes.join("|"))
            },
            if paths.is_empty() {
                String::new()
            } else {
                format!(" under {}", paths.join(", "))
            },
            if a.include_agents {
                ", agent approvals count"
            } else {
                ", humans only"
            }
        ));
    }
    if !b.agents_may_land_without_human.is_empty() {
        let v: Vec<&str> = b
            .agents_may_land_without_human
            .iter()
            .map(|c| c.as_str())
            .collect();
        out.push(format!(
            "agents may land without a human approval: {}",
            v.join(", ")
        ));
    } else {
        out.push("agents need a human approval".into());
    }
    if b.dismiss_stale_approvals {
        out.push("approvals of older revisions are dismissed".into());
    }
    if b.require_last_push_approval {
        out.push("someone other than the last pusher must approve".into());
    }
    if b.require_linear {
        out.push("linear stacks only".into());
    }
    if b.require_signed {
        out.push("signed changesets only".into());
    }
    out
}

/// Re-evaluates auto-merge on the events that can satisfy its requirements.
pub struct AutoMergeTrigger {
    changes: Arc<Changes>,
}

impl AutoMergeTrigger {
    /// A trigger over `changes`.
    pub fn new(changes: Arc<Changes>) -> AutoMergeTrigger {
        AutoMergeTrigger { changes }
    }

    async fn ids(&self, ev: &Event) -> Vec<String> {
        match (ev.kind.as_str(), &ev.body) {
            ("check.updated", Some(event::Body::Check(run))) => {
                let hex = run.changeset.as_ref().map(hex_of).unwrap_or_default();
                self.changes
                    .auto_merge_for_head(&ev.repo_id, &hex)
                    .await
                    .unwrap_or_default()
            }
            ("review.submitted" | "review.dismissed", Some(event::Body::Review(r))) => {
                vec![r.change_id.clone()]
            }
            ("cr.ready_for_review" | "cr.revised", Some(event::Body::Change(cr))) => {
                vec![cr.id.clone()]
            }
            ("cr.landed", _) => serde_json::from_str::<serde_json::Value>(&ev.payload_json)
                .ok()
                .and_then(|v| {
                    v.get("retargeted").and_then(|r| r.as_array()).map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl EventListener for AutoMergeTrigger {
    async fn on_event(&self, ev: &Event) {
        let ids = self.ids(ev).await;
        if ids.is_empty() {
            return;
        }
        let Ok(repo) = self.changes.orgs.repo_any(&Cx::system(), &ev.repo_id).await else {
            return;
        };
        for id in ids {
            let _ = self.changes.try_auto_merge(&repo, &id).await;
        }
    }
}

/// Extension for counting merge-queue lands in the throughput window.
trait CountWindow {
    async fn count_window(&mut self, scope: &str, now: u64) -> Result<u64>;
}

impl CountWindow for Tx {
    async fn count_window(&mut self, scope: &str, now: u64) -> Result<u64> {
        let rows = self
            .scan(
                &QUEUE_LOG,
                &Scan::scope(scope, 100_000)
                    .by_n1()
                    .n1_min(now.saturating_sub(THROUGHPUT_WINDOW_MS) as i64),
            )
            .await?;
        Ok(rows.len() as u64)
    }
}

/// Milliseconds of a timestamp option (0 when unset).
pub(crate) fn ms(t: &Option<pbjson_types::Timestamp>) -> u64 {
    t.as_ref().map(ts_ms).unwrap_or(0)
}
