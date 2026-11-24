//! [`Changes`]: the change-request store and its lifecycle.
//!
//! - **Open** a CR from a head changeset: the stack is the first-parent chain from the head
//!   back to the first ancestor of the target tip (or of the parent CR's head for stacked
//!   CRs, `depends_on`). The CR gets the next per-repo number, revision 1 and its diff class.
//! - **Revise** on every new head (amend, rebase, new commits): a new revision with base,
//!   head and diff class; review threads are carried forward or marked outdated; with the
//!   land rule's `dismiss_stale_approvals`, approvals of older revisions are dismissed.
//! - **Close**, **reopen** (never a landed CR), **edit** (title, body, target, parent; a new
//!   base records a new revision), drafts and ready-for-review, summaries, labels,
//!   assignees, milestone and linked issues (closing keywords in the body and the stack's
//!   messages link issues automatically), conversation locking, reactions, conversation and
//!   changeset comments, stacks and templates.
//!
//! Every state change is one transaction on the `changes` surface that records its events
//! and its audit entry.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_proto::v1::{
    ChangeBrief, ChangeRequest, ChangeRevision, ChangeState, ChangeTemplate, Comment, Event, Label,
    Milestone, ObjectId, Principal, PrincipalKind, ReactionContent, Repo, RepoRef, ReviewRequest,
    SubjectKind, SubjectRef, TeamReviewAssignment, event, repo_ref,
};

use super::model::{CrRecord, PathClass, RevMeta, reaction_groups, toggle_reaction};
use super::text;
use super::vcs::{ChangeVcs, Lander, StackInfo};
use super::{CHANGESET_COMMENTS, COMMENTS, CRS, SURFACE};
use crate::audit::AuditDraft;
use crate::checks::Checks;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::hooks::{PolicySource, PrincipalDirectory, RevisionResolver};
use crate::ids::{hex, ts};
use crate::leases::Leases;
use crate::orgs::Orgs;
use crate::store::{Db, Key, Row, Scan, Tx};

/// Longest title, in bytes.
pub const MAX_TITLE: usize = 1024;
/// Longest body, comment or summary, in bytes.
pub const MAX_BODY: usize = 256 * 1024;
/// Most labels, assignees, reviewers or linked issues on one CR.
pub const MAX_ITEMS: usize = 100;
/// Most CRs one list call examines.
pub const MAX_EXAMINE: usize = 10_000;

/// Serializes state changes that can invalidate a landing decision for one CR.
pub(crate) async fn lock_change_request(tx: &mut Tx, repo_id: &str, id: &str) -> Result<()> {
    tx.add_n1(
        &super::COUNTERS,
        repo_id,
        &format!("change_request:{id}"),
        0,
    )
    .await?;
    Ok(())
}

/// Team membership and review-assignment settings (the enterprise module owns teams).
#[async_trait]
pub trait TeamDirectory: Send + Sync {
    /// Members of a team.
    async fn members(&self, org_id: &str, team_id: &str) -> Result<Vec<Principal>>;
    /// The team's automatic review assignment, if configured.
    async fn assignment(&self, org_id: &str, team_id: &str)
    -> Result<Option<TeamReviewAssignment>>;
}

type TeamEntry = (Vec<Principal>, Option<TeamReviewAssignment>);

/// An in-memory [`TeamDirectory`].
#[derive(Default)]
pub struct MemoryTeams {
    map: RwLock<BTreeMap<(String, String), TeamEntry>>,
}

impl MemoryTeams {
    /// An empty directory.
    pub fn new() -> MemoryTeams {
        MemoryTeams::default()
    }

    /// Sets a team's members and assignment settings.
    pub fn set_team(
        &self,
        org_id: &str,
        team_id: &str,
        members: Vec<Principal>,
        assignment: Option<TeamReviewAssignment>,
    ) {
        if let Ok(mut m) = self.map.write() {
            m.insert((org_id.into(), team_id.into()), (members, assignment));
        }
    }
}

#[async_trait]
impl TeamDirectory for MemoryTeams {
    async fn members(&self, org_id: &str, team_id: &str) -> Result<Vec<Principal>> {
        Ok(self
            .map
            .read()
            .ok()
            .and_then(|m| m.get(&(org_id.into(), team_id.into())).map(|e| e.0.clone()))
            .unwrap_or_default())
    }

    async fn assignment(
        &self,
        org_id: &str,
        team_id: &str,
    ) -> Result<Option<TeamReviewAssignment>> {
        Ok(self.map.read().ok().and_then(|m| {
            m.get(&(org_id.into(), team_id.into()))
                .and_then(|e| e.1.clone())
        }))
    }
}

/// What the changes module needs beyond the forge core.
pub struct ChangeHooks {
    /// VCS reads and writes.
    pub vcs: Arc<dyn ChangeVcs>,
    /// Landing (the server's coalescer).
    pub lander: Arc<dyn Lander>,
    /// Teams for team review requests.
    pub teams: Arc<dyn TeamDirectory>,
}

/// A new change request.
#[derive(Clone, Debug, Default)]
pub struct NewChange {
    /// Target ref (empty = the repo's default ref).
    pub target_ref: String,
    /// Stack head.
    pub head: Option<ObjectId>,
    /// Title (empty = the head's message subject).
    pub title: String,
    /// Body (empty = the named template's body, if any).
    pub body: String,
    /// Parent CR for stacked CRs.
    pub depends_on: String,
    /// Open as a draft.
    pub draft: bool,
    /// Principals to request reviews from.
    pub reviewer_ids: Vec<String>,
    /// Teams to request reviews from.
    pub reviewer_team_ids: Vec<String>,
    /// Label names.
    pub labels: Vec<String>,
    /// Assignees.
    pub assignee_ids: Vec<String>,
    /// Milestone id.
    pub milestone_id: String,
    /// Linked issue numbers.
    pub linked_issues: Vec<u64>,
    /// Template name.
    pub template: String,
    /// The CR this one reverts.
    pub revert_of: String,
    /// The fork the head comes from (empty = this repo); the caller copied its stack in.
    pub head_repo_id: String,
    /// The ref the CR comes from, in `head_repo_id` (else this repo).
    pub head_ref: String,
    /// Fork CRs: target-repo writers may update `head_ref`.
    pub allow_maintainer_edits: bool,
    /// The post that inspired the change, already checked by
    /// [`crate::posts::Posts::inspiration`]; recorded as an adoption with the change.
    pub inspired_by: Option<bee_proto::v1::PostRef>,
    /// The brief written with the CR (SPEC §11.9; [`super::brief::validate`] rules), checked
    /// before anything is opened.
    pub brief: Option<ChangeBrief>,
}

/// A partial edit of a CR.
#[derive(Clone, Debug, Default)]
pub struct ChangeEdit {
    /// New title.
    pub title: Option<String>,
    /// New body.
    pub body: Option<String>,
    /// New target ref.
    pub target_ref: Option<String>,
    /// New parent CR (empty clears).
    pub depends_on: Option<String>,
    /// Expected CR version (0 = any).
    pub expected_version: u64,
    /// Fork CRs: whether target-repo writers may update the fork's head ref.
    pub allow_maintainer_edits: Option<bool>,
}

/// Change requests over the store.
pub struct Changes {
    pub(crate) db: Db,
    pub(crate) vcs: Arc<dyn ChangeVcs>,
    pub(crate) lander: Arc<dyn Lander>,
    pub(crate) teams: Arc<dyn TeamDirectory>,
    pub(crate) policy: Option<Arc<dyn PolicySource>>,
    pub(crate) directory: Arc<dyn PrincipalDirectory>,
    pub(crate) resolver: Arc<dyn RevisionResolver>,
    pub(crate) checks: Arc<Checks>,
    pub(crate) leases: Arc<Leases>,
    pub(crate) orgs: Arc<Orgs>,
    pub(crate) work: Arc<crate::work::Work>,
    pub(crate) facts: std::sync::RwLock<Option<Arc<dyn super::vcs::LandFacts>>>,
    pub(crate) posts: std::sync::OnceLock<Arc<crate::posts::Posts>>,
}

/// Zero-padded CR number (row id).
pub(crate) fn pad(n: u64) -> String {
    format!("{n:020}")
}

/// Hex of an object id.
pub(crate) fn hex_of(o: &ObjectId) -> String {
    hex(&o.raw)
}

/// A repo reference by id.
pub(crate) fn repo_ref(repo: &Repo) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(repo.id.clone())),
    })
}

fn check_len(field: &str, s: &str, max: usize) -> Result<()> {
    if s.len() > max {
        return Err(ForgeError::too_large(field, max as u64, s.len() as u64));
    }
    Ok(())
}

/// `TOO_LARGE` when a body-like field is longer than [`MAX_BODY`].
pub(crate) fn check_body(field: &str, s: &str) -> Result<()> {
    check_len(field, s, MAX_BODY)
}

fn check_count(field: &str, n: usize) -> Result<()> {
    if n > MAX_ITEMS {
        return Err(ForgeError::too_large(field, MAX_ITEMS as u64, n as u64));
    }
    Ok(())
}

fn state_of(cr: &ChangeRequest) -> ChangeState {
    ChangeState::try_from(cr.state).unwrap_or(ChangeState::Unspecified)
}

/// Fails with `INVALID_STATE` unless the CR is open.
pub(crate) fn require_open(cr: &ChangeRequest) -> Result<()> {
    match state_of(cr) {
        ChangeState::Open => Ok(()),
        ChangeState::Landed => Err(ForgeError::invalid_state(format!(
            "change request #{} has landed",
            cr.number
        ))),
        _ => Err(ForgeError::invalid_state(format!(
            "change request #{} is closed",
            cr.number
        ))),
    }
}

/// The current head of a CR.
pub(crate) fn head_of(cr: &ChangeRequest) -> Option<&ObjectId> {
    cr.revisions.last().and_then(|r| r.head.as_ref())
}

/// The current revision number of a CR.
pub(crate) fn current_rev(cr: &ChangeRequest) -> u32 {
    cr.revisions.last().map(|r| r.n).unwrap_or(0)
}

/// JSON summary for audit entries.
pub(crate) fn cr_json(cr: &ChangeRequest) -> String {
    serde_json::json!({
        "id": cr.id,
        "number": cr.number,
        "state": state_of(cr).as_str_name(),
        "target_ref": cr.target_ref,
        "title": cr.title,
        "draft": cr.draft,
        "revision": current_rev(cr),
        "head": head_of(cr).map(hex_of).unwrap_or_default(),
        "depends_on": cr.depends_on,
    })
    .to_string()
}

impl Changes {
    /// The changes module over `forge`'s store, checks, leases, orgs, policy source,
    /// directory and revision resolver, plus `hooks`.
    pub fn new(forge: &Forge, hooks: ChangeHooks) -> Arc<Changes> {
        Arc::new(Changes {
            db: forge.db.clone(),
            vcs: hooks.vcs,
            lander: hooks.lander,
            teams: hooks.teams,
            policy: forge.hooks.policy.clone(),
            directory: forge.hooks.directory.clone(),
            resolver: forge.hooks.resolver.clone(),
            checks: forge.checks.clone(),
            leases: forge.leases.clone(),
            orgs: forge.orgs.clone(),
            work: forge.work.clone(),
            facts: std::sync::RwLock::new(None),
            posts: std::sync::OnceLock::new(),
        })
    }

    /// Installs the land facts (signatures, required workflows, code-scanning alerts).
    pub fn set_land_facts(&self, facts: Arc<dyn super::vcs::LandFacts>) {
        *self.facts.write().unwrap_or_else(|p| p.into_inner()) = Some(facts);
    }

    /// Installs the posts module (`crate::posts::Posts::attach`): lands publish, closes
    /// discard and reopens restore a change request's post, and reads carry it. Only the first
    /// call takes effect.
    pub fn set_posts(&self, posts: Arc<crate::posts::Posts>) {
        let _ = self.posts.set(posts);
    }

    /// The installed posts module, if any.
    pub fn posts(&self) -> Option<&Arc<crate::posts::Posts>> {
        self.posts.get()
    }

    /// The installed land facts.
    pub(crate) fn land_facts(&self) -> Option<Arc<dyn super::vcs::LandFacts>> {
        self.facts.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Registers the auto-merge trigger on `forge`'s event log: `check.updated`,
    /// `review.submitted`, `cr.ready_for_review`, `cr.revised` and `cr.landed` events
    /// re-evaluate CRs with auto-merge enabled and land those policy now allows.
    pub fn attach(self: &Arc<Self>, forge: &Forge) {
        forge
            .events
            .add_listener(Arc::new(super::landing::AutoMergeTrigger::new(
                self.clone(),
            )));
    }

    /// The VCS hook.
    pub fn vcs(&self) -> &Arc<dyn ChangeVcs> {
        &self.vcs
    }

    // ----- rows ---------------------------------------------------------------------------

    pub(crate) async fn load(tx: &mut Tx, repo_id: &str, id: &str) -> Result<(CrRecord, Row)> {
        if id.is_empty() {
            return Err(ForgeError::missing("id"));
        }
        let rows = tx
            .scan_msgs::<CrRecord>(&CRS, &Scan::scope(repo_id, 1).key(Key::K1(id.to_string())))
            .await?;
        rows.into_iter()
            .next()
            .ok_or_else(|| ForgeError::not_found(format!("change request {id}")))
    }

    pub(crate) async fn load_number(
        tx: &mut Tx,
        repo_id: &str,
        number: u64,
    ) -> Result<(CrRecord, Row)> {
        tx.get_msg::<CrRecord>(&CRS, repo_id, &pad(number))
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("change request #{number}")))
    }

    /// Writes a record back (CAS on the read version); returns the new version.
    pub(crate) async fn save(tx: &mut Tx, mut rec: CrRecord, row: &Row) -> Result<(CrRecord, u64)> {
        let now = tx.now_ms();
        let linked = rec.linked_issues();
        let cr = rec.cr_mut();
        cr.updated_at = Some(ts(now));
        cr.linked_issues = linked;
        let head = head_of(cr).map(hex_of).unwrap_or_default();
        let mut next = row.clone().n1(now as i64).msg(&rec);
        next.k2 = Some(head);
        let v = tx.update(&CRS, &next).await?;
        Ok((rec, v))
    }

    /// The CR as served to `viewer`.
    pub(crate) fn served(rec: &CrRecord, version: u64, viewer: &str) -> ChangeRequest {
        let mut cr = rec.cr().clone();
        cr.version = version;
        cr.comment_count = rec.comment_count;
        cr.reactions = reaction_groups(&rec.reactions, viewer);
        cr.linked_issues = rec.linked_issues();
        cr.closing_issues = rec.closing_issues();
        for r in &mut cr.revisions {
            if r.pusher.is_none() {
                r.pusher = rec
                    .revs
                    .iter()
                    .find(|m| m.n == r.n)
                    .and_then(|m| m.pusher.clone());
            }
        }
        if let Some(e) = cr.merge_queue_entry.as_mut() {
            e.land_method = rec.queue_method;
            e.on_behalf_of.clone_from(&rec.land_chain);
        }
        if let Some(a) = cr.auto_merge.as_mut() {
            a.on_behalf_of.clone_from(&rec.land_chain);
        }
        cr
    }

    pub(crate) async fn emit_cr(
        tx: &mut Tx,
        kind: &str,
        rec: &CrRecord,
        payload_json: String,
    ) -> Result<()> {
        let cr = rec.cr();
        tx.emit_event(Event {
            kind: kind.into(),
            repo_id: cr.repo_id.clone(),
            org_id: rec.org_id.clone(),
            body: Some(event::Body::Change(Self::served(rec, 0, ""))),
            subject: Some(SubjectRef {
                kind: SubjectKind::ChangeRequest as i32,
                repo: Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(cr.repo_id.clone())),
                }),
                id: cr.id.clone(),
            }),
            payload_json,
            ..Default::default()
        })
        .await
    }

    pub(crate) fn audit_cr(action: &str, rec: &CrRecord) -> AuditDraft {
        let cr = rec.cr();
        AuditDraft::new(action, &rec.org_id)
            .repo_id(&cr.repo_id)
            .target(SubjectKind::ChangeRequest, &cr.id)
            .after(&cr_json(cr))
    }

    /// Applies `f` to a CR in one transaction: loads it, runs `f` (which may emit events
    /// and must name the audit action), writes it back and commits. `f` returns the audit
    /// action, or `None` for no change (nothing is written).
    pub(crate) async fn mutate<F>(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        expected_version: u64,
        f: F,
    ) -> Result<ChangeRequest>
    where
        F: FnOnce(&mut CrRecord, &mut Vec<(&'static str, String)>) -> Result<Option<String>> + Send,
    {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        let before = cr_json(rec.cr());
        let mut events = Vec::new();
        let Some(action) = f(&mut rec, &mut events)? else {
            return Ok(Self::served(&rec, row.version, &cx.actor_id));
        };
        let (rec, v) = Self::save(&mut tx, rec, &row).await?;
        for (kind, payload) in events {
            Self::emit_cr(&mut tx, kind, &rec, payload).await?;
        }
        tx.audit(Self::audit_cr(&action, &rec).before(&before))
            .await?;
        tx.commit().await?;
        Ok(Self::served(&rec, v, &cx.actor_id))
    }

    /// Applies a mutation that must serialize with landing authorization.
    pub(crate) async fn mutate_guarded<F>(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        expected_version: u64,
        f: F,
    ) -> Result<ChangeRequest>
    where
        F: FnOnce(&mut CrRecord, &mut Vec<(&'static str, String)>) -> Result<Option<String>> + Send,
    {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        lock_change_request(&mut tx, &repo.id, id).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        let expired = rec.clear_expired_landing_intent(tx.now_ms())?;
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        let before = cr_json(rec.cr());
        let mut events = Vec::new();
        let Some(action) = f(&mut rec, &mut events)? else {
            if expired {
                let (rec, v) = Self::save(&mut tx, rec, &row).await?;
                tx.commit().await?;
                return Ok(Self::served(&rec, v, &cx.actor_id));
            }
            return Ok(Self::served(&rec, row.version, &cx.actor_id));
        };
        let (rec, v) = Self::save(&mut tx, rec, &row).await?;
        for (kind, payload) in events {
            Self::emit_cr(&mut tx, kind, &rec, payload).await?;
        }
        tx.audit(Self::audit_cr(&action, &rec).before(&before))
            .await?;
        tx.commit().await?;
        Ok(Self::served(&rec, v, &cx.actor_id))
    }

    // ----- lookups ------------------------------------------------------------------------

    /// A principal with its display data from the directory.
    pub(crate) async fn principal(&self, id: &str) -> Principal {
        match self.directory.get(id).await {
            Some(p) => p,
            None => Principal {
                id: id.to_string(),
                ..Default::default()
            },
        }
    }

    /// The acting principal with display data.
    pub(crate) async fn actor(&self, cx: &Cx) -> Principal {
        let mut p = self.principal(&cx.actor_id).await;
        if p.kind == 0 {
            p.kind = cx.actor_kind as i32;
        }
        p
    }

    /// The repo's effective policy, when a policy source is installed.
    pub(crate) async fn policy_of(&self, repo: &Repo) -> Result<Option<bee_policy::Policy>> {
        match &self.policy {
            Some(src) => src.policy(repo).await,
            None => Ok(None),
        }
    }

    /// Classification rules and their fingerprint from the policy.
    pub(crate) fn class_rules(
        policy: Option<&bee_policy::Policy>,
    ) -> Result<(bee_classify::ClassRules, String)> {
        let Some(p) = policy else {
            return Ok((bee_classify::ClassRules::default(), String::new()));
        };
        let gen_: Vec<String> = p
            .doc()
            .paths
            .generated
            .iter()
            .map(|g| g.as_str().to_string())
            .collect();
        let sens: Vec<String> = p
            .doc()
            .paths
            .sensitive
            .iter()
            .map(|g| g.as_str().to_string())
            .collect();
        let rules = bee_classify::ClassRules::with_policy(gen_, sens)
            .map_err(|e| ForgeError::invalid("invalid_policy", e.message))?;
        Ok((rules, p.epoch().to_string()))
    }

    /// Reads a CR.
    pub async fn get(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<ChangeRequest> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        Ok(Self::served(&rec, row.version, &cx.actor_id))
    }

    /// Reads a CR by number.
    pub async fn get_by_number(&self, cx: &Cx, repo: &Repo, number: u64) -> Result<ChangeRequest> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load_number(&mut tx, &repo.id, number).await?;
        Ok(Self::served(&rec, row.version, &cx.actor_id))
    }

    /// A CR's record and version.
    pub(crate) async fn record(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<(CrRecord, u64)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        Ok((rec, row.version))
    }

    /// Every CR record of a repo, newest first (at most [`MAX_EXAMINE`]).
    pub(crate) async fn records(&self, cx: &Cx, repo_id: &str) -> Result<Vec<(CrRecord, u64)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<CrRecord>(&CRS, &Scan::scope(repo_id, MAX_EXAMINE).desc())
            .await?
            .into_iter()
            .map(|(r, row)| (r, row.version))
            .collect())
    }

    // ----- stack computation ----------------------------------------------------------------

    /// Where a CR's stack sits: the parent CR's head for stacked CRs, else the target tip.
    pub(crate) async fn onto(
        &self,
        cx: &Cx,
        repo: &Repo,
        target_ref: &str,
        depends_on: &str,
    ) -> Result<ObjectId> {
        if !depends_on.is_empty() {
            let (parent, _) = self.record(cx, repo, depends_on).await?;
            let p = parent.cr();
            if state_of(p) == ChangeState::Open {
                if p.target_ref != target_ref {
                    return Err(ForgeError::invalid(
                        "stack_target_mismatch",
                        format!(
                            "the parent CR #{} targets {}, not {target_ref}",
                            p.number, p.target_ref
                        ),
                    ));
                }
                return head_of(p).cloned().ok_or_else(|| {
                    ForgeError::internal(format!("CR #{} has no revision", p.number))
                });
            }
        }
        let tip = self
            .vcs
            .ref_tip(repo, target_ref)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("ref {target_ref}")))?;
        tip.target
            .ok_or_else(|| ForgeError::not_found(format!("ref {target_ref}")))
    }

    /// The stack and classification of `head` onto `onto`.
    pub(crate) async fn analyze(
        &self,
        repo: &Repo,
        head: &ObjectId,
        onto: &ObjectId,
    ) -> Result<(StackInfo, String, Vec<PathClass>, Vec<String>)> {
        let stack = self.vcs.stack(repo, head, onto).await?;
        let policy = self.policy_of(repo).await?;
        let (rules, fp) = Self::class_rules(policy.as_ref())?;
        let classified = self.vcs.classify(repo, &stack.members, &rules, &fp).await?;
        let paths = classified
            .paths
            .iter()
            .map(|p| PathClass {
                path: p.path.clone(),
                class: p.class.clone(),
                size: p.size.unwrap_or(0),
                deleted: p.size.is_none(),
            })
            .collect();
        let mut messages = Vec::new();
        for m in stack.members.iter().take(MAX_ITEMS) {
            messages.push(self.vcs.commit(repo, m).await?.message);
        }
        Ok((stack, classified.highest, paths, messages))
    }

    /// Closing-keyword issues of a body and messages.
    pub(crate) fn keyword_issues(repo: &Repo, body: &str, messages: &[String]) -> Vec<u64> {
        let mut v = text::closing_issues(body, &repo.full_name);
        for m in messages {
            v.extend(text::closing_issues(m, &repo.full_name));
        }
        v.sort_unstable();
        v.dedup();
        v
    }

    // ----- lifecycle ----------------------------------------------------------------------

    /// Opens a CR.
    pub async fn open(&self, cx: &Cx, repo: &Repo, new: NewChange) -> Result<ChangeRequest> {
        let head = new
            .head
            .clone()
            .ok_or_else(|| ForgeError::missing("head"))?;
        check_len("title", &new.title, MAX_TITLE)?;
        check_len("body", &new.body, MAX_BODY)?;
        check_count(
            "reviewers",
            new.reviewer_ids.len() + new.reviewer_team_ids.len(),
        )?;
        check_count("labels", new.labels.len())?;
        check_count("assignees", new.assignee_ids.len())?;
        check_count("linked_issues", new.linked_issues.len())?;
        let brief = new.brief.as_ref().map(super::brief::validate).transpose()?;
        let target_ref = if new.target_ref.is_empty() {
            repo.default_ref.clone()
        } else {
            new.target_ref.clone()
        };
        let onto = self.onto(cx, repo, &target_ref, &new.depends_on).await?;
        let (stack, highest, paths, messages) = self.analyze(repo, &head, &onto).await?;
        let title = if new.title.trim().is_empty() {
            messages
                .last()
                .and_then(|m| m.lines().next())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| ForgeError::missing("title"))?
        } else {
            new.title.trim().to_string()
        };
        let body = if new.body.is_empty() && !new.template.is_empty() {
            self.templates(cx, repo, &target_ref)
                .await?
                .into_iter()
                .find(|t| t.name == new.template)
                .map(|t| t.body)
                .ok_or_else(|| ForgeError::not_found(format!("template {}", new.template)))?
        } else {
            new.body.clone()
        };
        let author = self.actor(cx).await;
        let mut requests = Vec::new();
        let now_ms = self.db.now_ms();
        for r in &new.reviewer_ids {
            if *r != cx.actor_id {
                requests.push(ReviewRequest {
                    reviewer: Some(self.principal(r).await),
                    requested_by: Some(author.clone()),
                    requested_at: Some(ts(now_ms)),
                    ..Default::default()
                });
            }
        }
        for t in &new.reviewer_team_ids {
            requests.push(ReviewRequest {
                team_id: t.clone(),
                requested_by: Some(author.clone()),
                requested_at: Some(ts(now_ms)),
                ..Default::default()
            });
        }
        let mut assignees = Vec::new();
        for a in &new.assignee_ids {
            assignees.push(self.principal(a).await);
        }
        let labels = labels_of(&new.labels);
        let id = self.db.ids().ulid()?;
        let revision = ChangeRevision {
            n: 1,
            head: Some(head.clone()),
            base: Some(stack.base.clone()),
            diff_class: highest,
            created_at: Some(ts(now_ms)),
            pusher: Some(author.clone()),
        };
        let cr = ChangeRequest {
            id: id.clone(),
            repo_id: repo.id.clone(),
            target_ref,
            change_ids: stack.change_ids.clone(),
            state: ChangeState::Open as i32,
            author: Some(author.clone()),
            title,
            body: body.clone(),
            revisions: vec![revision],
            depends_on: new.depends_on.clone(),
            created_at: Some(ts(now_ms)),
            draft: new.draft,
            labels,
            assignees,
            milestone: (!new.milestone_id.is_empty()).then(|| Milestone {
                id: new.milestone_id.clone(),
                ..Default::default()
            }),
            review_requests: Vec::new(),
            revert_of: new.revert_of.clone(),
            head_repo_id: new.head_repo_id.clone(),
            head_ref: new.head_ref.clone(),
            allow_maintainer_edits: new.allow_maintainer_edits && !new.head_repo_id.is_empty(),
            brief: brief
                .clone()
                .map(|b| Box::new(super::brief::stamped(b, 1, author.clone(), now_ms))),
            ..Default::default()
        };
        let mut rec = CrRecord {
            cr: Some(cr),
            org_id: repo.org_id.clone(),
            paths,
            revs: vec![RevMeta {
                n: 1,
                pusher: Some(author.clone()),
            }],
            keyword_issues: Self::keyword_issues(repo, &body, &messages),
            has_merges: stack.has_merges,
            stack: stack.members.clone(),
            manual_issues: new.linked_issues.clone(),
            ..CrRecord::default()
        };
        // Code owners and team expansion, unless the CR starts as a draft.
        if !new.draft {
            requests.extend(self.owner_requests(repo, &rec, &requests).await?);
        }
        let requests = self.expand_teams(cx, repo, &rec, requests).await?;
        rec.cr_mut().review_requests = requests;
        // Issues and CRs share one per-repo number space, owned by the work module.
        let number = self
            .work
            .allocate_number(cx, &repo.id, crate::work::NumberKind::ChangeRequest, &id)
            .await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        rec.cr_mut().number = number;
        rec.cr_mut().updated_at = Some(ts(now_ms));
        let linked = rec.linked_issues();
        rec.cr_mut().linked_issues = linked;
        let row = Row::new(repo.id.clone(), pad(number))
            .k1(id.clone())
            .k2(hex_of(&head))
            .n1(now_ms as i64)
            .msg(&rec);
        tx.insert(&CRS, &row).await?;
        if let Some(post) = &new.inspired_by {
            crate::posts::adopt::record(
                &mut tx,
                &crate::posts::adopt::AdoptionRec {
                    post: Some(post.clone()),
                    repo_id: repo.id.clone(),
                    org_id: repo.org_id.clone(),
                    kind: bee_proto::v1::SubjectKind::ChangeRequest as i32,
                    subject_id: id.clone(),
                    number,
                    title: rec.cr().title.clone(),
                    author: Some(author.clone()),
                    created_ms: now_ms,
                },
            )
            .await?;
        }
        Self::emit_cr(&mut tx, "cr.opened", &rec, String::new()).await?;
        if brief.is_some() {
            Self::emit_cr(&mut tx, "cr.brief_set", &rec, String::new()).await?;
        }
        if !rec.cr().review_requests.is_empty() {
            Self::emit_cr(&mut tx, "cr.review_requested", &rec, String::new()).await?;
        }
        tx.audit(Self::audit_cr("cr.open", &rec)).await?;
        tx.commit().await?;
        Ok(Self::served(&rec, 1, &cx.actor_id))
    }

    /// Records a new head as a new revision (no-op when the head is unchanged).
    pub async fn revise(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        head: &ObjectId,
    ) -> Result<ChangeRequest> {
        self.revise_with(cx, repo, id, Some(head), None).await
    }

    /// Records a new revision for a new head and/or a new target or parent.
    pub(crate) async fn revise_with(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        head: Option<&ObjectId>,
        retarget: Option<(String, String)>,
    ) -> Result<ChangeRequest> {
        for _ in 0..5 {
            let (rec, version) = self.record(cx, repo, id).await?;
            let cr = rec.cr().clone();
            require_open(&cr)?;
            let old_head = head_of(&cr)
                .cloned()
                .ok_or_else(|| ForgeError::internal("CR without revision"))?;
            let new_head = head.cloned().unwrap_or_else(|| old_head.clone());
            let (target_ref, depends_on) = retarget
                .clone()
                .unwrap_or_else(|| (cr.target_ref.clone(), cr.depends_on.clone()));
            let onto = self.onto(cx, repo, &target_ref, &depends_on).await?;
            let (stack, highest, paths, messages) = self.analyze(repo, &new_head, &onto).await?;
            let old_base = cr
                .revisions
                .last()
                .and_then(|r| r.base.clone())
                .unwrap_or_default();
            if new_head == old_head && stack.base == old_base && retarget.is_none() {
                return Ok(Self::served(&rec, version, &cx.actor_id));
            }
            let n = current_rev(&cr) + 1;
            let threads = self
                .carry_threads(cx, repo, &rec, &new_head, &stack.base, n)
                .await?;
            let policy = self.policy_of(repo).await?;
            let dismiss_stale = policy
                .as_ref()
                .and_then(|p| land_rule(p, &target_ref))
                .is_some_and(|b| b.dismiss_stale_approvals);
            match self
                .commit_revision(
                    cx,
                    repo,
                    id,
                    version,
                    RevisionUpdate {
                        head: new_head.clone(),
                        stack,
                        highest,
                        paths,
                        messages,
                        n,
                        threads,
                        dismiss_stale,
                        retarget: retarget.clone(),
                    },
                )
                .await
            {
                Err(e) if e.reason == bee_proto::v1::ErrorReason::VersionMismatch => continue,
                other => return other,
            }
        }
        Err(ForgeError::new(
            bee_proto::v1::ErrorReason::VersionMismatch,
            "the change request kept changing; retry",
        ))
    }

    async fn commit_revision(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        version: u64,
        u: RevisionUpdate,
    ) -> Result<ChangeRequest> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        lock_change_request(&mut tx, &repo.id, id).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        rec.clear_expired_landing_intent(tx.now_ms())?;
        if row.version != version {
            return Err(ForgeError::version_mismatch(version, row.version));
        }
        let before = cr_json(rec.cr());
        let pusher = self.actor(cx).await;
        let now = tx.now_ms();
        let body = rec.cr().body.clone();
        {
            let cr = rec.cr_mut();
            cr.revisions.push(ChangeRevision {
                n: u.n,
                head: Some(u.head.clone()),
                base: Some(u.stack.base.clone()),
                diff_class: u.highest.clone(),
                created_at: Some(ts(now)),
                pusher: Some(pusher.clone()),
            });
            cr.change_ids = u.stack.change_ids.clone();
            if let Some((t, d)) = &u.retarget {
                cr.target_ref = t.clone();
                cr.depends_on = d.clone();
            }
        }
        rec.paths = u.paths;
        rec.stack = u.stack.members.clone();
        rec.has_merges = u.stack.has_merges;
        rec.keyword_issues = Self::keyword_issues(repo, &body, &u.messages);
        rec.revs.push(RevMeta {
            n: u.n,
            pusher: Some(pusher),
        });
        // A new revision leaves the merge queue.
        let left_queue = self
            .drop_queue_entry(&mut tx, repo, &mut rec, "revised")
            .await?;
        let (rec, v) = Self::save(&mut tx, rec, &row).await?;
        self.store_threads(&mut tx, repo, &rec, u.threads).await?;
        self.mark_stale_reviews(&mut tx, &rec, u.n, u.dismiss_stale)
            .await?;
        if left_queue {
            Self::emit_cr(&mut tx, "merge_queue.left", &rec, String::new()).await?;
        }
        Self::emit_cr(&mut tx, "cr.revised", &rec, String::new()).await?;
        if u.retarget.is_some() {
            Self::emit_cr(&mut tx, "cr.edited", &rec, String::new()).await?;
        }
        tx.audit(Self::audit_cr("cr.revise", &rec).before(&before))
            .await?;
        tx.commit().await?;
        Ok(Self::served(&rec, v, &cx.actor_id))
    }

    /// Closes an open CR.
    pub async fn close(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        reason: &str,
    ) -> Result<ChangeRequest> {
        check_len("reason", reason, MAX_TITLE)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        require_open(rec.cr())?;
        let before = cr_json(rec.cr());
        let now = tx.now_ms();
        {
            let cr = rec.cr_mut();
            cr.state = ChangeState::Closed as i32;
            cr.close_reason = reason.to_string();
            cr.closed_at = Some(ts(now));
            cr.auto_merge = None;
        }
        let left = self
            .drop_queue_entry(&mut tx, repo, &mut rec, "closed")
            .await?;
        let (rec, v) = Self::save(&mut tx, rec, &row).await?;
        if left {
            Self::emit_cr(&mut tx, "merge_queue.left", &rec, String::new()).await?;
        }
        Self::emit_cr(&mut tx, "cr.closed", &rec, String::new()).await?;
        crate::posts::store::on_closed(&mut tx, &rec).await?;
        tx.audit(Self::audit_cr("cr.close", &rec).before(&before))
            .await?;
        tx.commit().await?;
        Ok(Self::served(&rec, v, &cx.actor_id))
    }

    /// Reopens a closed CR (`INVALID_STATE` for open and landed CRs); its discarded post is a
    /// draft again.
    pub async fn reopen(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<ChangeRequest> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        match state_of(rec.cr()) {
            ChangeState::Closed => {}
            ChangeState::Landed => {
                return Err(ForgeError::invalid_state(
                    "a landed change request cannot be reopened",
                ));
            }
            _ => return Err(ForgeError::invalid_state("the change request is open")),
        }
        let before = cr_json(rec.cr());
        {
            let cr = rec.cr_mut();
            cr.state = ChangeState::Open as i32;
            cr.closed_at = None;
            cr.close_reason.clear();
        }
        let (rec, v) = Self::save(&mut tx, rec, &row).await?;
        Self::emit_cr(&mut tx, "cr.reopened", &rec, String::new()).await?;
        crate::posts::store::on_reopened(&mut tx, &rec).await?;
        tx.audit(Self::audit_cr("cr.reopen", &rec).before(&before))
            .await?;
        tx.commit().await?;
        Ok(Self::served(&rec, v, &cx.actor_id))
    }

    /// Edits title, body, target ref or parent CR. A new target or parent recomputes the
    /// stack and records a new revision.
    pub async fn edit(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        edit: ChangeEdit,
    ) -> Result<ChangeRequest> {
        if let Some(t) = &edit.title {
            check_len("title", t, MAX_TITLE)?;
            if t.trim().is_empty() {
                return Err(ForgeError::missing("title"));
            }
        }
        if let Some(b) = &edit.body {
            check_len("body", b, MAX_BODY)?;
        }
        let (rec, version) = self.record(cx, repo, id).await?;
        if edit.expected_version != 0 && edit.expected_version != version {
            return Err(ForgeError::version_mismatch(edit.expected_version, version));
        }
        let cur = rec.cr().clone();
        let target = edit.target_ref.clone().unwrap_or(cur.target_ref.clone());
        let parent = edit.depends_on.clone().unwrap_or(cur.depends_on.clone());
        if parent == cur.id {
            return Err(ForgeError::invalid(
                "stack_cycle",
                "a change request cannot depend on itself",
            ));
        }
        if !parent.is_empty() && parent != cur.depends_on {
            // Refuse cycles: walk the new parent's chain.
            let mut at = parent.clone();
            for _ in 0..MAX_ITEMS {
                if at.is_empty() {
                    break;
                }
                if at == cur.id {
                    return Err(ForgeError::invalid(
                        "stack_cycle",
                        "the new parent depends on this change request",
                    ));
                }
                at = self.record(cx, repo, &at).await?.0.cr().depends_on.clone();
            }
        }
        if target != cur.target_ref || parent != cur.depends_on {
            self.revise_with(cx, repo, id, None, Some((target, parent)))
                .await?;
        }
        let messages = {
            let mut m = Vec::new();
            for s in rec.stack.iter().take(MAX_ITEMS) {
                m.push(self.vcs.commit(repo, s).await?.message);
            }
            m
        };
        let repo_c = repo.clone();
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            let mut changed = false;
            if let Some(t) = edit.title.map(|t| t.trim().to_string())
                && t != rec.cr().title
            {
                rec.cr_mut().title = t;
                changed = true;
            }
            if let Some(b) = edit.body
                && b != rec.cr().body
            {
                rec.keyword_issues = Self::keyword_issues(&repo_c, &b, &messages);
                rec.cr_mut().body = b;
                changed = true;
            }
            if let Some(m) = edit.allow_maintainer_edits
                && m != rec.cr().allow_maintainer_edits
            {
                if rec.cr().head_repo_id.is_empty() {
                    return Err(ForgeError::invalid(
                        "not_a_fork_change",
                        "maintainer edits apply to change requests from a fork",
                    ));
                }
                rec.cr_mut().allow_maintainer_edits = m;
                changed = true;
            }
            if !changed {
                return Ok(None);
            }
            ev.push(("cr.edited", String::new()));
            Ok(Some("cr.edit".into()))
        })
        .await
    }

    /// Marks a draft ready for review (requesting code owners) or turns a CR into a draft.
    pub async fn set_draft(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        draft: bool,
    ) -> Result<ChangeRequest> {
        let (rec, _) = self.record(cx, repo, id).await?;
        require_open(rec.cr())?;
        let extra = if !draft && rec.cr().draft {
            let owners = self
                .owner_requests(repo, &rec, &rec.cr().review_requests)
                .await?;
            self.expand_teams(cx, repo, &rec, owners).await?
        } else {
            Vec::new()
        };
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            require_open(rec.cr())?;
            if rec.cr().draft == draft {
                return Ok(None);
            }
            let cr = rec.cr_mut();
            cr.draft = draft;
            if draft {
                ev.push(("cr.converted_to_draft", String::new()));
                return Ok(Some("cr.convert_to_draft".into()));
            }
            let added = merge_requests(&mut cr.review_requests, extra);
            ev.push(("cr.ready_for_review", String::new()));
            if added {
                ev.push(("cr.review_requested", String::new()));
            }
            Ok(Some("cr.ready_for_review".into()))
        })
        .await
    }

    /// Sets the summary (agent- or human-authored).
    pub async fn set_summary(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        summary: &str,
    ) -> Result<ChangeRequest> {
        check_len("summary", summary, MAX_BODY)?;
        let author = self.actor(cx).await;
        let summary = summary.to_string();
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            let cr = rec.cr_mut();
            cr.summary = summary;
            cr.summary_author = Some(author);
            ev.push(("cr.summary_set", String::new()));
            Ok(Some("cr.set_summary".into()))
        })
        .await
    }

    /// Replaces the labels (names; the issues module owns their definitions).
    pub async fn set_labels(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        labels: &[String],
    ) -> Result<ChangeRequest> {
        check_count("labels", labels.len())?;
        let labels = labels_of(labels);
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            if rec.cr().labels == labels {
                return Ok(None);
            }
            rec.cr_mut().labels = labels;
            ev.push(("cr.labels_changed", String::new()));
            Ok(Some("cr.set_labels".into()))
        })
        .await
    }

    /// Replaces the assignees.
    pub async fn set_assignees(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        ids: &[String],
    ) -> Result<ChangeRequest> {
        check_count("assignees", ids.len())?;
        let mut seen = BTreeSet::new();
        let mut assignees = Vec::new();
        for a in ids {
            if seen.insert(a.clone()) {
                assignees.push(self.principal(a).await);
            }
        }
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            if rec.cr().assignees == assignees {
                return Ok(None);
            }
            rec.cr_mut().assignees = assignees;
            ev.push(("cr.assignees_changed", String::new()));
            Ok(Some("cr.set_assignees".into()))
        })
        .await
    }

    /// Sets or clears (empty id) the milestone.
    pub async fn set_milestone(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        milestone_id: &str,
    ) -> Result<ChangeRequest> {
        let m = (!milestone_id.is_empty()).then(|| Milestone {
            id: milestone_id.to_string(),
            ..Default::default()
        });
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            if rec.cr().milestone == m {
                return Ok(None);
            }
            rec.cr_mut().milestone = m;
            ev.push(("cr.milestone_changed", String::new()));
            Ok(Some("cr.set_milestone".into()))
        })
        .await
    }

    /// Links and unlinks issues; `close_on_land` makes manual links close on land.
    pub async fn link_issues(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        add: &[u64],
        remove: &[u64],
        close_on_land: bool,
    ) -> Result<ChangeRequest> {
        check_count("linked_issues", add.len() + remove.len())?;
        if add.contains(&0) {
            return Err(ForgeError::invalid(
                "invalid_issue",
                "issue numbers start at 1",
            ));
        }
        let (add, remove) = (add.to_vec(), remove.to_vec());
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            let mut set: BTreeSet<u64> = rec.manual_issues.iter().copied().collect();
            set.extend(add.iter().copied());
            for r in &remove {
                set.remove(r);
            }
            rec.keyword_issues.retain(|i| !remove.contains(i));
            if set.len() > MAX_ITEMS {
                return Err(ForgeError::too_large(
                    "linked_issues",
                    MAX_ITEMS as u64,
                    set.len() as u64,
                ));
            }
            rec.manual_issues = set.into_iter().collect();
            rec.close_on_land = close_on_land;
            let payload = serde_json::json!({
                "linked_issues": rec.linked_issues(),
                "closes_issues": rec.closing_issues(),
            })
            .to_string();
            ev.push(("cr.issues_linked", payload));
            Ok(Some("cr.link_issues".into()))
        })
        .await
    }

    /// Locks or unlocks the conversation (only people who can moderate comment then).
    pub async fn set_locked(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        locked: bool,
    ) -> Result<ChangeRequest> {
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            if rec.cr().locked == locked {
                return Ok(None);
            }
            rec.cr_mut().locked = locked;
            ev.push((
                if locked { "cr.locked" } else { "cr.unlocked" },
                String::new(),
            ));
            Ok(Some(if locked { "cr.lock" } else { "cr.unlock" }.into()))
        })
        .await
    }

    /// Adds or removes the caller's reaction on the CR.
    pub async fn react(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        content: ReactionContent,
        add: bool,
    ) -> Result<ChangeRequest> {
        if content == ReactionContent::Unspecified {
            return Err(ForgeError::missing("content"));
        }
        let who = cx.actor_id.clone();
        self.mutate(cx, repo, id, 0, move |rec, ev| {
            if !toggle_reaction(&mut rec.reactions, &who, content as i32, add) {
                return Ok(None);
            }
            ev.push((
                "cr.reacted",
                serde_json::json!({
                    "content": content.as_str_name(),
                    "added": add,
                })
                .to_string(),
            ));
            Ok(Some("cr.react".into()))
        })
        .await
    }

    /// The CRs of a stack, bottom first: the chain of parents of `id`, `id`, then every
    /// descendant (breadth first).
    pub async fn stack(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Vec<ChangeRequest>> {
        let all = self.records(cx, &repo.id).await?;
        let by_id: BTreeMap<String, &(CrRecord, u64)> =
            all.iter().map(|r| (r.0.cr().id.clone(), r)).collect();
        let start = by_id
            .get(id)
            .ok_or_else(|| ForgeError::not_found(format!("change request {id}")))?;
        let mut down = Vec::new();
        let mut at = start.0.cr().depends_on.clone();
        let mut seen = BTreeSet::from([id.to_string()]);
        while let Some(r) = by_id.get(&at) {
            if !seen.insert(at.clone()) {
                break;
            }
            down.push(*r);
            at = r.0.cr().depends_on.clone();
        }
        down.reverse();
        down.push(start);
        let mut queue = vec![id.to_string()];
        while let Some(parent) = queue.pop() {
            let mut kids: Vec<&(CrRecord, u64)> = all
                .iter()
                .filter(|r| r.0.cr().depends_on == parent)
                .collect();
            kids.sort_by_key(|r| r.0.cr().number);
            for k in kids {
                if seen.insert(k.0.cr().id.clone()) {
                    queue.insert(0, k.0.cr().id.clone());
                    down.push(k);
                }
            }
        }
        Ok(down
            .into_iter()
            .map(|(r, v)| Self::served(r, *v, &cx.actor_id))
            .collect())
    }

    /// CR templates read from the tip of `target_ref` (empty = default ref).
    pub async fn templates(
        &self,
        _cx: &Cx,
        repo: &Repo,
        target_ref: &str,
    ) -> Result<Vec<ChangeTemplate>> {
        let target = if target_ref.is_empty() {
            repo.default_ref.as_str()
        } else {
            target_ref
        };
        let Some(tip) = self.vcs.ref_tip(repo, target).await?.and_then(|r| r.target) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for f in text::TEMPLATE_FILES {
            if out.iter().any(|t: &ChangeTemplate| t.name == "default") {
                break;
            }
            if let Some(b) = self.vcs.read_file(repo, &tip, f).await? {
                out.push(ChangeTemplate {
                    name: text::template_name(f),
                    path: f.as_bytes().to_vec(),
                    body: String::from_utf8_lossy(&b).into_owned(),
                });
            }
        }
        for d in text::TEMPLATE_DIRS {
            let mut names = self.vcs.list_dir(repo, &tip, d).await?;
            names.sort();
            for n in names.into_iter().filter(|n| n.ends_with(".md")) {
                let path = format!("{d}/{n}");
                if let Some(b) = self.vcs.read_file(repo, &tip, &path).await? {
                    out.push(ChangeTemplate {
                        name: text::template_name(&path),
                        path: path.into_bytes(),
                        body: String::from_utf8_lossy(&b).into_owned(),
                    });
                }
            }
        }
        Ok(out)
    }

    // ----- conversation and changeset comments -------------------------------------------------

    /// Adds a conversation comment. A locked CR accepts comments only when `moderator`.
    pub async fn comment(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        body: &str,
        moderator: bool,
    ) -> Result<Comment> {
        if body.trim().is_empty() {
            return Err(ForgeError::missing("body"));
        }
        check_len("body", body, MAX_BODY)?;
        let author = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
        if rec.cr().locked && !moderator {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::Locked,
                "the conversation is locked",
            ));
        }
        let now = tx.now_ms();
        let cid = tx.new_id()?;
        let comment = Comment {
            id: cid.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::ChangeRequest as i32,
                repo: repo_ref(repo),
                id: id.to_string(),
            }),
            author: Some(author),
            body: body.to_string(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        tx.insert(
            &COMMENTS,
            &Row::new(id, cid.clone())
                .n1(now as i64)
                .msg(&super::model::CommentRecord {
                    conversation: Some(comment.clone()),
                    ..Default::default()
                }),
        )
        .await?;
        super::comments::index_comment(&mut tx, &cid, &repo.id, id).await?;
        rec.comment_count += 1;
        let (rec, _) = Self::save(&mut tx, rec, &row).await?;
        let json = serde_json::to_string(&comment).unwrap_or_default();
        Self::emit_cr(&mut tx, "cr_comment.created", &rec, json.clone()).await?;
        tx.audit(
            AuditDraft::new("cr.comment", &rec.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Comment, &cid)
                .after(&json),
        )
        .await?;
        tx.commit().await?;
        Ok(comment)
    }

    /// Conversation comments, oldest first, paged by comment id.
    pub async fn comments(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Comment>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::load(&mut tx, &repo.id, id).await?;
        let after = (!token.is_empty()).then(|| crate::store::Cursor {
            n1: 0,
            scope: id.to_string(),
            id: token.to_string(),
        });
        let mut out = Vec::new();
        let mut next = String::new();
        let mut cursor = after;
        'outer: loop {
            let rows = tx
                .scan_msgs::<super::model::CommentRecord>(
                    &COMMENTS,
                    &Scan::scope(id, 500).after(cursor.clone()),
                )
                .await?;
            if rows.is_empty() {
                break;
            }
            for (c, row) in &rows {
                cursor = Some(crate::store::Cursor::after(row));
                if let Some(mut conv) = c.conversation.clone() {
                    conv.reactions = reaction_groups(&c.reactions, &cx.actor_id);
                    out.push(conv);
                    if out.len() >= limit {
                        next = row.id.clone();
                        break 'outer;
                    }
                }
            }
            if rows.len() < 500 {
                break;
            }
        }
        Ok((out, next))
    }

    /// Adds or removes the caller's reaction on a comment of the CR.
    pub async fn react_comment(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        comment_id: &str,
        content: ReactionContent,
        add: bool,
    ) -> Result<Vec<bee_proto::v1::ReactionGroup>> {
        if content == ReactionContent::Unspecified {
            return Err(ForgeError::missing("content"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, id).await?;
        let (mut c, row) = tx
            .get_msg::<super::model::CommentRecord>(&COMMENTS, id, comment_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("comment {comment_id}")))?;
        if !toggle_reaction(&mut c.reactions, &cx.actor_id, content as i32, add) {
            return Ok(reaction_groups(&c.reactions, &cx.actor_id));
        }
        tx.update(&COMMENTS, &row.clone().msg(&c)).await?;
        Self::emit_cr(
            &mut tx,
            "cr.reacted",
            &rec,
            serde_json::json!({
                "comment_id": comment_id,
                "content": content.as_str_name(),
                "added": add,
            })
            .to_string(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("cr.react", &rec.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Comment, comment_id),
        )
        .await?;
        tx.commit().await?;
        Ok(reaction_groups(&c.reactions, &cx.actor_id))
    }

    /// Adds a comment on a changeset outside any CR (a commit comment), optionally anchored
    /// to a path and line.
    pub async fn changeset_comment(
        &self,
        cx: &Cx,
        repo: &Repo,
        changeset: &ObjectId,
        body: &str,
        path: &[u8],
        line: u32,
    ) -> Result<Comment> {
        if changeset.raw.is_empty() {
            return Err(ForgeError::missing("changeset"));
        }
        if body.trim().is_empty() {
            return Err(ForgeError::missing("body"));
        }
        check_len("body", body, MAX_BODY)?;
        // The changeset must exist in the repo.
        self.vcs.commit(repo, changeset).await?;
        if !path.is_empty() {
            let p = std::str::from_utf8(path)
                .map_err(|_| ForgeError::invalid("invalid_path", "paths must be UTF-8"))?;
            if self.vcs.file_id(repo, changeset, p).await?.is_none() {
                return Err(ForgeError::invalid(
                    "invalid_anchor",
                    format!("{p} is not a file in the changeset"),
                ));
            }
        }
        let author = self.actor(cx).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let cid = tx.new_id()?;
        let comment = Comment {
            id: cid.clone(),
            subject: Some(SubjectRef {
                kind: SubjectKind::Changeset as i32,
                repo: repo_ref(repo),
                id: hex_of(changeset),
            }),
            author: Some(author),
            body: body.to_string(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        // The anchor is kept next to the comment and served in `Comment.anchor`.
        let review = (!path.is_empty()).then(|| bee_proto::v1::ReviewComment {
            id: cid.clone(),
            path: path.to_vec(),
            side: "new".into(),
            start_line: line,
            end_line: line,
            file_level: line == 0,
            ..Default::default()
        });
        let record = super::model::CommentRecord {
            conversation: Some(comment.clone()),
            review: review.clone(),
            ..Default::default()
        };
        let mut comment = comment;
        comment.anchor = review.as_ref().map(anchor_of);
        tx.insert(
            &CHANGESET_COMMENTS,
            &Row::new(repo.id.clone(), cid.clone())
                .k1(hex_of(changeset))
                .n1(now as i64)
                .msg(&record),
        )
        .await?;
        let json = serde_json::to_string(&comment).unwrap_or_default();
        tx.emit_event(Event {
            kind: "changeset_comment.created".into(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            subject: comment.subject.clone(),
            payload_json: json.clone(),
            ..Default::default()
        })
        .await?;
        tx.audit(
            AuditDraft::new("changeset_comment.create", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::Comment, &cid)
                .after(&json),
        )
        .await?;
        tx.commit().await?;
        Ok(comment)
    }

    /// Comments on a changeset, oldest first, paged by comment id.
    pub async fn changeset_comments(
        &self,
        cx: &Cx,
        repo: &Repo,
        changeset: &ObjectId,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Comment>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let after = (!token.is_empty()).then(|| crate::store::Cursor {
            n1: 0,
            scope: repo.id.clone(),
            id: token.to_string(),
        });
        let rows = tx
            .scan_msgs::<super::model::CommentRecord>(
                &CHANGESET_COMMENTS,
                &Scan::scope(repo.id.clone(), limit + 1)
                    .key(Key::K1(hex_of(changeset)))
                    .after(after),
            )
            .await?;
        let more = rows.len() > limit;
        let page: Vec<(super::model::CommentRecord, Row)> = rows.into_iter().take(limit).collect();
        let next = if more {
            page.last().map(|(_, r)| r.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        Ok((
            page.into_iter()
                .filter_map(|(c, _)| {
                    let mut conv = c.conversation?;
                    conv.anchor = c.review.as_ref().map(anchor_of);
                    Some(conv)
                })
                .collect(),
            next,
        ))
    }
}

/// Inputs of a new revision.
pub(crate) struct RevisionUpdate {
    pub head: ObjectId,
    pub stack: StackInfo,
    pub highest: String,
    pub paths: Vec<PathClass>,
    pub messages: Vec<String>,
    pub n: u32,
    pub threads: Vec<(bee_proto::v1::ReviewThread, Row)>,
    pub dismiss_stale: bool,
    pub retarget: Option<(String, String)>,
}

/// Labels from names (sorted, unique, non-empty).
pub(crate) fn labels_of(names: &[String]) -> Vec<Label> {
    let set: BTreeSet<String> = names
        .iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    set.into_iter()
        .map(|name| Label {
            name,
            ..Default::default()
        })
        .collect()
}

/// Adds requests not already present; returns whether any was added.
pub(crate) fn merge_requests(cur: &mut Vec<ReviewRequest>, add: Vec<ReviewRequest>) -> bool {
    let mut added = false;
    for r in add {
        let dup = cur.iter().any(|c| same_request(c, &r));
        if !dup {
            cur.push(r);
            added = true;
        }
    }
    added
}

/// Whether two requests name the same reviewer or team.
pub(crate) fn same_request(a: &ReviewRequest, b: &ReviewRequest) -> bool {
    match (&a.reviewer, &b.reviewer) {
        (Some(x), Some(y)) => x.id == y.id,
        (None, None) => a.team_id == b.team_id,
        _ => false,
    }
}

/// The most specific land rule body for a ref.
pub(crate) fn land_rule<'a>(
    policy: &'a bee_policy::Policy,
    ref_name: &str,
) -> Option<&'a bee_policy::RuleBody> {
    bee_policy::most_specific(policy.doc().land.iter(), |(p, _)| p, ref_name).map(|(_, r)| &r.body)
}

/// The policy kind of a principal kind.
pub(crate) fn policy_kind(k: i32) -> bee_policy::PrincipalKind {
    match PrincipalKind::try_from(k).unwrap_or(PrincipalKind::Human) {
        PrincipalKind::Agent => bee_policy::PrincipalKind::Agent,
        PrincipalKind::Service => bee_policy::PrincipalKind::Service,
        _ => bee_policy::PrincipalKind::Human,
    }
}

/// The anchor of a line or file comment as `Comment.anchor`.
pub(crate) fn anchor_of(c: &bee_proto::v1::ReviewComment) -> bee_proto::v1::CommentAnchor {
    bee_proto::v1::CommentAnchor {
        path: c.path.clone(),
        side: c.side.clone(),
        start_line: c.start_line,
        end_line: c.end_line,
        file_level: c.file_level,
        revision: c.revision,
        thread_id: c.thread_id.clone(),
        outdated: c.outdated,
    }
}
