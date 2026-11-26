//! Post-land bisect and automatic revert (SPEC §11.6, PRODUCT §4.14, scenario AS-CI-02).
//!
//! When a `post_land` check run fails on the default ref, [`BisectListener`] starts a bisect:
//! the candidates are the changesets on the ref's first-parent chain after the last one where
//! the same check passed (at most [`BisectConfig::max_range`]); outcomes already reported for
//! candidates are used as they are. Each round re-runs the failing workflow job at evenly
//! spaced candidates through a [`BisectProber`] ([`CiProber`]: a probe run of the job and its
//! needs, which reports no check runs) with at most [`BisectConfig::parallelism`] probes at a
//! time; the plan is a pure function of the outcomes ([`plan`]). Before any of that an
//! automatic session re-runs the failing check once at the failing changeset (a `confirm`
//! probe): if it passes the failure is flaky, the session ends `FLAKY` (`bisect.flaky`) and
//! nothing is blamed. A failing check run starts at most one session. When the first failing
//! changeset is found the session records it, maps it to the change request whose land put it
//! on the ref ([`Changes::landed_by`], also mid-stack), notifies the author of that CR (a `bisect.found` event about that CR, a comment on it and a review request on the
//! revert), and opens a revert CR through the changes domain linking the failing run. With the
//! repo setting `auto_revert` the revert is landed at once (or put on auto-merge, or into the
//! merge queue, when policy does not let it land yet).
//!
//! Manual sessions ([`Bisector::start`] with `manual`) are driven by good/bad marks
//! ([`Bisector::mark`]); [`Bisector::next_candidate`] names the changeset to test next.
//!
//! Sessions live in `ci::BISECTS`. Every change is one transaction with its audit entry
//! (`bisect.*`) and events (`bisect.started`, `bisect.found`, `bisect.finished`,
//! `bisect.reverted`, `bisect.flaky`). Probes advance on `workflow_run.completed` events and on
//! [`Bisector::tick`], which the server runs with the CI worker. `CheckService.StartBisect`,
//! `GetBisect` and `ListBisects` are served by [`service::BisectCheckService`].

pub mod model;
pub mod plan;
pub mod prober;
pub mod service;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use bee_proto::v1::{
    Bisect, BisectState, ChangeState, CheckRun, CheckStatus, Event, LandMethod, ObjectId, Repo,
    SubjectKind, SubjectRef, event,
};
use tokio::sync::Mutex as AsyncMutex;

pub use model::{BisectRecord, ProbeIntent, ProbeRecord};
pub use plan::{Outcome, Plan, Status};
pub use prober::{BisectProber, CiProber, ProbeTarget};

use crate::audit::AuditDraft;
use crate::changes::Changes;
use crate::checks::{Checks, RunFilter, policy_status};
use crate::ci::{BISECTS, Ci, SURFACE};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::events::EventListener;
use crate::forge::Forge;
use crate::ids::{hex, ts, unhex};
use crate::orgs::Orgs;
use crate::store::{Db, Key, Row, Scan, Tx};

/// Bisect settings.
#[derive(Clone, Debug)]
pub struct BisectConfig {
    /// Probes running at once per session.
    pub parallelism: u32,
    /// Most changesets a session looks back for the last green one.
    pub max_range: usize,
    /// Rounds after which a session gives up.
    pub max_rounds: u32,
}

impl Default for BisectConfig {
    fn default() -> BisectConfig {
        BisectConfig {
            parallelism: 2,
            max_range: 256,
            max_rounds: 32,
        }
    }
}

/// A request to bisect a ref.
#[derive(Clone, Debug, Default)]
pub struct StartBisect {
    /// The ref (bee name or `refs/heads/...`; default: the repo's default ref).
    pub git_ref: String,
    /// The failing check.
    pub check_name: String,
    /// Last known good changeset (default: the last one where the check passed).
    pub good: Option<ObjectId>,
    /// Known bad changeset (default: the ref's tip).
    pub bad: Option<ObjectId>,
    /// Open a revert CR for the culprit.
    pub open_revert: bool,
    /// Drive the session with marks instead of probes.
    pub manual: bool,
}

/// One probe or mark as seen by API callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeView {
    /// The candidate.
    pub changeset: ObjectId,
    /// Its outcome; `None` while the probe runs.
    pub outcome: Option<Outcome>,
    /// Round (0 = history or a mark).
    pub round: u32,
    /// The prober's handle (a workflow run id for CI probes).
    pub probe_id: String,
    /// `probe`, `history` or the principal that marked it.
    pub source: String,
}

/// A bisect session in full.
#[derive(Clone, Debug, PartialEq)]
pub struct BisectSession {
    /// The public view.
    pub bisect: Bisect,
    /// Candidates, oldest first (the last is the failing changeset).
    pub candidates: Vec<ObjectId>,
    /// Probes and marks in order.
    pub probes: Vec<ProbeView>,
    /// Rounds launched.
    pub rounds: u32,
    /// `auto` or `manual`.
    pub mode: String,
    /// Why it ended as it did, and what happened to the revert.
    pub note: String,
    /// The CR that landed the culprit.
    pub culprit_change_request_id: String,
    /// Its author.
    pub culprit_author: String,
    /// The revert landed.
    pub revert_landed: bool,
    /// Where the failure that started it can be seen.
    pub failing_url: String,
}

fn state_key(s: BisectState) -> &'static str {
    match s {
        BisectState::Running => "running",
        BisectState::Found => "found",
        BisectState::Inconclusive => "inconclusive",
        BisectState::Cancelled => "cancelled",
        BisectState::Flaky => "flaky",
        BisectState::Unspecified => "unspecified",
    }
}

fn state_of(b: &Bisect) -> BisectState {
    BisectState::try_from(b.state).unwrap_or(BisectState::Unspecified)
}

fn oid(hexs: &str) -> ObjectId {
    ObjectId {
        raw: unhex(hexs).unwrap_or_default(),
    }
}

fn bisect_row(repo_id: &str, rec: &BisectRecord, version: u64) -> Row {
    let b = rec.bisect.clone().unwrap_or_default();
    let started = b.started_at.as_ref().map(crate::ids::ts_ms).unwrap_or(0);
    let row = Row::new(repo_id, b.id.clone())
        .k1(state_key(state_of(&b)))
        .k2(b.r#ref.clone())
        .n1(started as i64)
        .msg(rec);
    if version > 0 {
        row.version(version)
    } else {
        row
    }
}

/// `ProbeRecord::source` of the re-run of the failing changeset.
const CONFIRM: &str = "confirm";

fn plan_of(rec: &BisectRecord) -> (Plan, BTreeSet<usize>) {
    let mut plan = Plan::new(rec.candidates.len());
    let mut pending = BTreeSet::new();
    for p in rec.probes.iter().filter(|p| p.source != CONFIRM) {
        match Outcome::parse(&p.result) {
            Some(o) => plan.record(p.index as usize, o),
            None => {
                pending.insert(p.index as usize);
            }
        }
    }
    (plan, pending)
}

fn session_of(rec: &BisectRecord) -> BisectSession {
    BisectSession {
        bisect: rec.bisect.clone().unwrap_or_default(),
        candidates: rec.candidates.iter().map(|c| oid(c)).collect(),
        probes: rec
            .probes
            .iter()
            .map(|p| ProbeView {
                changeset: oid(&p.changeset),
                outcome: Outcome::parse(&p.result),
                round: p.round,
                probe_id: p.probe_id.clone(),
                source: p.source.clone(),
            })
            .collect(),
        rounds: rec.rounds,
        mode: rec.mode.clone(),
        note: rec.note.clone(),
        culprit_change_request_id: rec.culprit_change_request_id.clone(),
        culprit_author: rec.culprit_author.clone(),
        revert_landed: rec.revert_landed,
        failing_url: rec.failing_url.clone(),
    }
}

fn failed(run: &CheckRun) -> bool {
    policy_status(CheckStatus::try_from(run.status).unwrap_or_default())
        == bee_policy::CheckStatus::Failure
}

/// Where a session starts from.
struct Origin {
    git_ref: String,
    check_name: String,
    bad: ObjectId,
    good: Option<ObjectId>,
    target: ProbeTarget,
    manual: bool,
    open_revert: bool,
    failing_check_run_id: String,
    failing_run_id: String,
    failing_url: String,
}

/// Post-land bisect sessions over CI and the changes domain.
pub struct Bisector {
    db: Db,
    orgs: Arc<Orgs>,
    checks: Arc<Checks>,
    ci: Arc<Ci>,
    changes: Arc<Changes>,
    prober: RwLock<Arc<dyn BisectProber>>,
    config: BisectConfig,
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl Bisector {
    /// A bisector over `forge`, its CI module and its changes domain, probing through CI.
    pub fn new(
        forge: &Forge,
        ci: Arc<Ci>,
        changes: Arc<Changes>,
        config: BisectConfig,
    ) -> Arc<Bisector> {
        Arc::new(Bisector {
            db: forge.db.clone(),
            orgs: forge.orgs.clone(),
            checks: forge.checks.clone(),
            prober: RwLock::new(Arc::new(CiProber::new(ci.clone()))),
            ci,
            changes,
            config,
            locks: Mutex::new(HashMap::new()),
        })
    }

    /// Registers [`BisectListener`] with `forge`'s event log.
    pub fn attach(self: &Arc<Self>, forge: &Forge) {
        forge
            .events
            .add_listener(Arc::new(BisectListener::new(self.clone())));
    }

    /// Replaces the prober (external CI systems re-run their own checks).
    pub fn set_prober(&self, prober: Arc<dyn BisectProber>) {
        if let Ok(mut g) = self.prober.write() {
            *g = prober;
        }
    }

    fn prober(&self) -> Arc<dyn BisectProber> {
        self.prober
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Settings.
    pub fn config(&self) -> &BisectConfig {
        &self.config
    }

    fn lock(&self, id: &str) -> Arc<AsyncMutex<()>> {
        let mut m = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(id.to_string()).or_default().clone()
    }

    fn default_ref(repo: &Repo) -> String {
        crate::ci::convert::bee_ref(if repo.default_ref.is_empty() {
            "main"
        } else {
            &repo.default_ref
        })
    }

    async fn load(tx: &mut Tx, repo_id: &str, id: &str) -> Result<(BisectRecord, Row)> {
        if id.is_empty() {
            return Err(ForgeError::missing("id"));
        }
        tx.get_msg::<BisectRecord>(&BISECTS, repo_id, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("bisect {id}")))
    }

    /// The latest outcome of `check` on `cs`, if one was reported.
    async fn check_outcome(
        &self,
        cx: &Cx,
        repo_id: &str,
        cs: &ObjectId,
        check: &str,
    ) -> Result<Option<Outcome>> {
        let runs = self
            .checks
            .list(
                cx,
                repo_id,
                &RunFilter {
                    changeset_hex: Some(hex(&cs.raw)),
                    name: check.to_string(),
                    latest_only: true,
                    ..RunFilter::default()
                },
            )
            .await?;
        Ok(runs.first().and_then(|r| {
            match policy_status(CheckStatus::try_from(r.status).unwrap_or_default()) {
                bee_policy::CheckStatus::Success => Some(Outcome::Pass),
                bee_policy::CheckStatus::Failure => Some(Outcome::Fail),
                _ => None,
            }
        }))
    }

    /// Walks the first-parent chain back from `bad`: the candidates (oldest first), the good
    /// changeset (given, or the last one where `check` passed) and outcomes already reported.
    async fn walk(
        &self,
        cx: &Cx,
        repo: &Repo,
        bad: &ObjectId,
        good: Option<&ObjectId>,
        check: &str,
    ) -> Result<(Vec<ObjectId>, Option<ObjectId>, Vec<(usize, Outcome)>)> {
        let vcs = self.changes.vcs();
        let mut chain = vec![bad.clone()];
        let mut known = Vec::new();
        let mut found_good = None;
        let mut cur = bad.clone();
        while chain.len() <= self.config.max_range {
            let Some(parent) = vcs.commit(repo, &cur).await?.parents.first().cloned() else {
                break;
            };
            match good {
                Some(g) if *g == parent => {
                    found_good = Some(parent);
                    break;
                }
                Some(_) => {}
                None => match self.check_outcome(cx, &repo.id, &parent, check).await? {
                    Some(Outcome::Pass) => {
                        found_good = Some(parent);
                        break;
                    }
                    Some(o) => known.push((chain.len(), o)),
                    None => {}
                },
            }
            chain.push(parent.clone());
            cur = parent;
        }
        if let Some(g) = good
            && found_good.is_none()
        {
            return Err(ForgeError::invalid(
                "good_not_ancestor",
                format!(
                    "{} is not a first-parent ancestor of {} within {} changesets",
                    hex(&g.raw),
                    hex(&bad.raw),
                    self.config.max_range
                ),
            ));
        }
        let n = chain.len();
        chain.reverse();
        let known = known.into_iter().map(|(i, o)| (n - 1 - i, o)).collect();
        Ok((chain, found_good, known))
    }

    /// Starts a session (`CheckService.StartBisect`, manual sessions). The check's failing
    /// workflow job is found from the latest run of `check_name` on the bad changeset; when
    /// no workflow job reports it the session is manual. Errors: NOT_FOUND (ref),
    /// INVALID_ARGUMENT (`no_known_good`, `good_not_ancestor`), INVALID_STATE (a session for
    /// the ref and check is already running).
    pub async fn start(&self, cx: &Cx, repo: &Repo, s: StartBisect) -> Result<Bisect> {
        if s.check_name.is_empty() {
            return Err(ForgeError::missing("check_name"));
        }
        let git_ref = if s.git_ref.is_empty() {
            Self::default_ref(repo)
        } else {
            crate::ci::convert::bee_ref(&s.git_ref)
        };
        let bad = match s.bad.filter(|b| !b.raw.is_empty()) {
            Some(b) => b,
            None => self
                .ci
                .vcs
                .ref_tip(repo, &git_ref)
                .await?
                .ok_or_else(|| ForgeError::not_found(format!("ref {git_ref}")))?,
        };
        if self.running_for(cx, repo, &git_ref, &s.check_name).await? {
            return Err(ForgeError::invalid_state(format!(
                "a bisect of {} on {git_ref} is already running",
                s.check_name
            )));
        }
        let failing = self
            .checks
            .list(
                cx,
                &repo.id,
                &RunFilter {
                    changeset_hex: Some(hex(&bad.raw)),
                    name: s.check_name.clone(),
                    latest_only: true,
                    ..RunFilter::default()
                },
            )
            .await?
            .into_iter()
            .next();
        let (target, run_id) = self
            .target_of(cx, repo, &git_ref, &s.check_name, failing.as_ref())
            .await;
        let manual = s.manual || target.workflow_path.is_empty();
        self.open_session(
            cx,
            repo,
            Origin {
                git_ref,
                check_name: s.check_name.clone(),
                bad,
                good: s.good.filter(|g| !g.raw.is_empty()),
                target,
                manual,
                open_revert: s.open_revert,
                failing_check_run_id: failing.as_ref().map(|f| f.id.clone()).unwrap_or_default(),
                failing_run_id: run_id,
                failing_url: failing.map(|f| f.url).unwrap_or_default(),
            },
        )
        .await
    }

    /// The probe target of a failing check: its workflow and job when a workflow job
    /// reported it; also the failing run's id.
    async fn target_of(
        &self,
        cx: &Cx,
        repo: &Repo,
        git_ref: &str,
        check: &str,
        failing: Option<&CheckRun>,
    ) -> (ProbeTarget, String) {
        let mut t = ProbeTarget {
            git_ref: git_ref.to_string(),
            check_name: check.to_string(),
            ..ProbeTarget::default()
        };
        let mut run_id = String::new();
        if let Some(f) = failing.filter(|f| !f.workflow_job_id.is_empty())
            && let Ok(j) = self.ci.job_record(cx, &repo.id, &f.workflow_job_id).await
        {
            let job = crate::ci::engine::job_of(&j).clone();
            if let Ok(run) = self.ci.run_record(cx, &repo.id, &job.run_id).await {
                t.workflow_path = run.workflow_key.clone();
                t.job_key = j.key.clone();
                run_id = job.run_id.clone();
            }
        }
        (t, run_id)
    }

    async fn running_for(&self, cx: &Cx, repo: &Repo, git_ref: &str, check: &str) -> Result<bool> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .scan_msgs::<BisectRecord>(
                &BISECTS,
                &Scan::scope(repo.id.clone(), 1000).key(Key::K1("running".into())),
            )
            .await?
            .iter()
            .any(|(r, _)| {
                r.bisect
                    .as_ref()
                    .is_some_and(|b| b.r#ref == git_ref && b.check_name == check)
            }))
    }

    async fn open_session(&self, cx: &Cx, repo: &Repo, o: Origin) -> Result<Bisect> {
        let (candidates, good, known) = self
            .walk(cx, repo, &o.bad, o.good.as_ref(), &o.check_name)
            .await?;
        if good.is_none() && o.manual {
            return Err(ForgeError::invalid(
                "no_known_good",
                format!(
                    "{} never passed in the last {} changesets of {}; pass `good`",
                    o.check_name, self.config.max_range, o.git_ref
                ),
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let id = tx.new_id()?;
        let (state, note) = if good.is_none() {
            (
                BisectState::Inconclusive,
                format!(
                    "{} has no passing run in the last {} changesets of {}; nothing to bisect against",
                    o.check_name, self.config.max_range, o.git_ref
                ),
            )
        } else {
            (BisectState::Running, String::new())
        };
        let bisect = Bisect {
            id: id.clone(),
            repo_id: repo.id.clone(),
            r#ref: o.git_ref.clone(),
            check_name: o.check_name.clone(),
            good: good.clone(),
            bad: Some(o.bad.clone()),
            state: state as i32,
            first_bad: None,
            tested: Vec::new(),
            revert_change_request_id: String::new(),
            started_at: Some(ts(now)),
            finished_at: (state != BisectState::Running).then(|| ts(now)),
        };
        let rec = BisectRecord {
            bisect: Some(bisect.clone()),
            candidates: candidates.iter().map(|c| hex(&c.raw)).collect(),
            probes: known
                .into_iter()
                .map(|(i, out)| ProbeRecord {
                    index: i as u32,
                    changeset: hex(&candidates[i].raw),
                    probe_id: String::new(),
                    result: out.as_str().into(),
                    round: 0,
                    source: "history".into(),
                })
                .collect(),
            rounds: 0,
            workflow_path: o.target.workflow_path.clone(),
            job_key: o.target.job_key.clone(),
            failing_check_run_id: o.failing_check_run_id.clone(),
            failing_run_id: o.failing_run_id.clone(),
            failing_url: o.failing_url.clone(),
            mode: if o.manual { "manual" } else { "auto" }.into(),
            open_revert: o.open_revert,
            parallelism: self.config.parallelism.max(1),
            note,
            culprit_change_request_id: String::new(),
            culprit_change_request_number: 0,
            culprit_author: String::new(),
            revert_landed: false,
            started_by: cx.actor_id.clone(),
            org_id: repo.org_id.clone(),
            confirm_tip: !o.manual,
            starting_probes: Vec::new(),
        };
        tx.insert(&BISECTS, &bisect_row(&repo.id, &rec, 0)).await?;
        tx.emit_event(Self::event("bisect.started", repo, &rec, None))
            .await?;
        if state != BisectState::Running {
            tx.emit_event(Self::event("bisect.finished", repo, &rec, None))
                .await?;
        }
        tx.audit(
            AuditDraft::new("bisect.start", &repo.org_id)
                .repo_id(&repo.id)
                .target(SubjectKind::CheckRun, &o.failing_check_run_id)
                .after(
                    &serde_json::json!({
                        "bisect": id, "ref": o.git_ref, "check": o.check_name,
                        "candidates": rec.candidates.len(), "mode": rec.mode,
                    })
                    .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        if state == BisectState::Running {
            return Box::pin(self.step(cx, repo, &id)).await;
        }
        Ok(bisect)
    }

    fn event(kind: &str, repo: &Repo, rec: &BisectRecord, subject: Option<SubjectRef>) -> Event {
        let b = rec.bisect.clone().unwrap_or_default();
        let payload = serde_json::json!({
            "bisect": serde_json::to_value(&b).unwrap_or_default(),
            "mode": rec.mode,
            "rounds": rec.rounds,
            "note": rec.note,
            "failing_url": rec.failing_url,
            "failing_run_id": rec.failing_run_id,
            "culprit_change_request_id": rec.culprit_change_request_id,
            "culprit_change_request_number": rec.culprit_change_request_number,
            "culprit_author": rec.culprit_author,
            "revert_change_request_id": b.revert_change_request_id,
            "revert_landed": rec.revert_landed,
        });
        Event {
            kind: kind.into(),
            repo_id: repo.id.clone(),
            org_id: repo.org_id.clone(),
            subject: subject.or_else(|| {
                Some(SubjectRef {
                    kind: SubjectKind::CheckRun as i32,
                    repo: crate::ci::engine::repo_ref_of(&repo.id),
                    id: rec.failing_check_run_id.clone(),
                })
            }),
            payload_json: payload.to_string(),
            ..Default::default()
        }
    }

    /// Starts an automatic session for a failed `post_land` check run on the default ref,
    /// unless one is running for the check or a found culprit's revert is still pending.
    pub async fn on_post_land_failure(
        &self,
        cx: &Cx,
        repo: &Repo,
        run: &CheckRun,
    ) -> Result<Option<Bisect>> {
        if !run.post_land || !failed(run) {
            return Ok(None);
        }
        let Some(cs) = run.changeset.clone().filter(|c| !c.raw.is_empty()) else {
            return Ok(None);
        };
        let git_ref = Self::default_ref(repo);
        let Some(tip) = self.ci.vcs.ref_tip(repo, &git_ref).await? else {
            return Ok(None);
        };
        let vcs = self.changes.vcs();
        if cs != tip && !vcs.is_ancestor(repo, &cs, &tip).await? {
            return Ok(None);
        }
        if self.running_for(cx, repo, &git_ref, &run.name).await? {
            return Ok(None);
        }
        let seen = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<BisectRecord>(&BISECTS, &Scan::scope(repo.id.clone(), 10_000))
                .await?
                .iter()
                .any(|(r, _)| r.failing_check_run_id == run.id)
        };
        if seen {
            // One session per failing run: a flaky verdict is not re-litigated by the same run.
            return Ok(None);
        }
        let found = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<BisectRecord>(
                &BISECTS,
                &Scan::scope(repo.id.clone(), 1000).key(Key::K1("found".into())),
            )
            .await?
        };
        for (r, _) in found {
            let b = r.bisect.clone().unwrap_or_default();
            if b.r#ref != git_ref || b.check_name != run.name || r.revert_landed {
                continue;
            }
            let Some(first_bad) = &b.first_bad else {
                continue;
            };
            if !vcs.is_ancestor(repo, first_bad, &cs).await? {
                continue;
            }
            let revert_open = !b.revert_change_request_id.is_empty()
                && self
                    .changes
                    .get(cx, repo, &b.revert_change_request_id)
                    .await
                    .is_ok_and(|cr| cr.state == ChangeState::Open as i32);
            if revert_open {
                return Ok(None);
            }
        }
        let (target, run_id) = self
            .target_of(cx, repo, &git_ref, &run.name, Some(run))
            .await;
        self.open_session(
            cx,
            repo,
            Origin {
                git_ref,
                check_name: run.name.clone(),
                bad: cs,
                good: None,
                target,
                manual: false,
                open_revert: true,
                failing_check_run_id: run.id.clone(),
                failing_run_id: run_id,
                failing_url: run.url.clone(),
            },
        )
        .await
        .map(Some)
    }

    /// Advances a session: collects finished probes, narrows the range, launches the next
    /// round, and finishes the session when the plan is decided.
    pub async fn step(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Bisect> {
        let lock = self.lock(id);
        let _g = lock.lock().await;
        let prober = self.prober();
        for _ in 0..(self.config.max_rounds as usize + 2) {
            let (mut rec, row, now_ms) = {
                let mut tx = self.db.begin(SURFACE, cx).await?;
                let (rec, row) = Self::load(&mut tx, &repo.id, id).await?;
                (rec, row, tx.now_ms())
            };
            let b = rec.bisect.clone().unwrap_or_default();
            if state_of(&b) != BisectState::Running {
                return Ok(b);
            }
            let target = ProbeTarget {
                git_ref: b.r#ref.clone(),
                check_name: b.check_name.clone(),
                workflow_path: rec.workflow_path.clone(),
                job_key: rec.job_key.clone(),
            };
            let mut changed = false;
            let now_ms = i64::try_from(now_ms).unwrap_or(i64::MAX);
            let stale_before = now_ms.saturating_sub(120_000);
            let intents_before = rec.starting_probes.len();
            rec.starting_probes
                .retain(|intent| intent.started_at_ms >= stale_before);
            changed |= rec.starting_probes.len() != intents_before;
            if rec.mode == "auto" {
                for p in rec.probes.iter_mut().filter(|p| p.result.is_empty()) {
                    let res = prober.poll(cx, repo, &target, &p.probe_id).await;
                    match res {
                        Ok(Some(o)) => {
                            p.result = o.as_str().into();
                            changed = true;
                        }
                        Ok(None) => {}
                        Err(_) => {
                            p.result = Outcome::Skip.as_str().into();
                            changed = true;
                        }
                    }
                }
            }
            if rec.mode == "auto" && rec.confirm_tip && !rec.candidates.is_empty() {
                let last = rec.candidates.len() - 1;
                match rec.probes.iter().find(|p| p.source == CONFIRM) {
                    None => {
                        if rec
                            .starting_probes
                            .iter()
                            .any(|intent| intent.source == CONFIRM)
                        {
                            return Ok(rec.bisect.unwrap_or_default());
                        }
                        rec.starting_probes.push(ProbeIntent {
                            index: last as u32,
                            round: 0,
                            source: CONFIRM.into(),
                            started_at_ms: now_ms,
                        });
                        let claim_version = match self.save(cx, repo, &rec, row.version).await {
                            Ok(version) => version,
                            Err(error)
                                if error.reason == bee_proto::v1::ErrorReason::VersionMismatch =>
                            {
                                return Ok(rec.bisect.unwrap_or_default());
                            }
                            Err(error) => return Err(error),
                        };
                        let bad = oid(&rec.candidates[last]);
                        let (probe_id, result) = match prober.start(cx, repo, &target, &bad).await {
                            Ok(pid) => (pid, String::new()),
                            Err(_) => (String::new(), Outcome::Skip.as_str().to_string()),
                        };
                        let started = result.is_empty();
                        rec.probes.push(ProbeRecord {
                            index: last as u32,
                            changeset: rec.candidates[last].clone(),
                            probe_id,
                            result,
                            round: 0,
                            source: CONFIRM.into(),
                        });
                        rec.starting_probes
                            .retain(|intent| intent.source != CONFIRM);
                        self.save(cx, repo, &rec, claim_version).await?;
                        if started {
                            return Ok(rec.bisect.unwrap_or_default());
                        }
                        continue;
                    }
                    Some(p) if p.result.is_empty() => {
                        if changed {
                            self.save(cx, repo, &rec, row.version).await?;
                        }
                        return Ok(rec.bisect.unwrap_or_default());
                    }
                    Some(p) if Outcome::parse(&p.result) == Some(Outcome::Pass) => {
                        let why = format!(
                            "`{}` passed when re-run at the failing changeset {}: the failure is flaky; nothing is blamed or reverted",
                            b.check_name, rec.candidates[last]
                        );
                        return self
                            .finish(cx, repo, rec, row.version, BisectState::Flaky, &why)
                            .await;
                    }
                    // Confirmed, or the re-run could not run: bisect.
                    Some(_) => {}
                }
            }
            let (plan, pending) = plan_of(&rec);
            let mut pending = pending;
            pending.extend(
                rec.starting_probes
                    .iter()
                    .filter(|intent| intent.source == "probe")
                    .map(|intent| intent.index as usize),
            );
            match plan.status(&pending) {
                Status::Found(i) => {
                    return Box::pin(self.finish_found(cx, repo, rec, row.version, i)).await;
                }
                Status::Contradiction(why) => {
                    return self
                        .finish(cx, repo, rec, row.version, BisectState::Inconclusive, &why)
                        .await;
                }
                Status::Stuck { lo, hi } => {
                    let why = format!(
                        "candidates {} to {} could not be tested; the culprit is one of them",
                        lo + 1,
                        hi + 1
                    );
                    return self
                        .finish(cx, repo, rec, row.version, BisectState::Inconclusive, &why)
                        .await;
                }
                Status::Open => {}
            }
            if rec.mode != "auto" || !pending.is_empty() {
                if changed {
                    self.save(cx, repo, &rec, row.version).await?;
                }
                return Ok(rec.bisect.unwrap_or_default());
            }
            if rec.rounds >= self.config.max_rounds {
                let why = format!("gave up after {} rounds", rec.rounds);
                return self
                    .finish(cx, repo, rec, row.version, BisectState::Inconclusive, &why)
                    .await;
            }
            let next = plan.next_round(rec.parallelism.max(1) as usize, &pending);
            if !rec.starting_probes.is_empty() {
                if changed {
                    self.save(cx, repo, &rec, row.version).await?;
                }
                return Ok(rec.bisect.unwrap_or_default());
            }
            if next.is_empty() {
                if changed {
                    self.save(cx, repo, &rec, row.version).await?;
                }
                return Ok(rec.bisect.unwrap_or_default());
            }
            rec.rounds += 1;
            for i in &next {
                rec.starting_probes.push(ProbeIntent {
                    index: *i as u32,
                    round: rec.rounds,
                    source: "probe".into(),
                    started_at_ms: now_ms,
                });
            }
            let mut version = match self.save(cx, repo, &rec, row.version).await {
                Ok(version) => version,
                Err(error) if error.reason == bee_proto::v1::ErrorReason::VersionMismatch => {
                    return Ok(rec.bisect.unwrap_or_default());
                }
                Err(error) => return Err(error),
            };
            let mut started = 0;
            for i in next {
                let cs = oid(&rec.candidates[i]);
                let (probe_id, result) = match prober.start(cx, repo, &target, &cs).await {
                    Ok(pid) => {
                        started += 1;
                        (pid, String::new())
                    }
                    Err(_) => (String::new(), Outcome::Skip.as_str().to_string()),
                };
                rec.probes.push(ProbeRecord {
                    index: i as u32,
                    changeset: rec.candidates[i].clone(),
                    probe_id,
                    result,
                    round: rec.rounds,
                    source: "probe".into(),
                });
                rec.starting_probes.retain(|intent| {
                    !(intent.source == "probe"
                        && intent.index == i as u32
                        && intent.round == rec.rounds)
                });
                version = self.save(cx, repo, &rec, version).await?;
            }
            if started > 0 {
                return Ok(rec.bisect.unwrap_or_default());
            }
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::load(&mut tx, &repo.id, id)
            .await?
            .0
            .bisect
            .unwrap_or_default())
    }

    async fn save(&self, cx: &Cx, repo: &Repo, rec: &BisectRecord, version: u64) -> Result<u64> {
        let mut b = rec.bisect.clone().unwrap_or_default();
        b.tested = rec
            .probes
            .iter()
            .filter(|p| matches!(p.result.as_str(), "pass" | "fail") && p.source != "history")
            .map(|p| oid(&p.changeset))
            .collect();
        let mut rec = rec.clone();
        rec.bisect = Some(b);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let version = tx
            .update(&BISECTS, &bisect_row(&repo.id, &rec, version))
            .await?;
        tx.commit().await.map(|()| version)
    }

    async fn finish(
        &self,
        cx: &Cx,
        repo: &Repo,
        mut rec: BisectRecord,
        version: u64,
        state: BisectState,
        note: &str,
    ) -> Result<Bisect> {
        let prober = self.prober();
        for p in rec.probes.iter().filter(|p| p.result.is_empty()) {
            let _ = prober.cancel(cx, repo, &p.probe_id).await;
        }
        for p in rec.probes.iter_mut().filter(|p| p.result.is_empty()) {
            p.result = Outcome::Skip.as_str().into();
        }
        let mut b = rec.bisect.clone().unwrap_or_default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        b.state = state as i32;
        b.finished_at = Some(ts(tx.now_ms()));
        rec.bisect = Some(b.clone());
        rec.note = note.to_string();
        tx.update(&BISECTS, &bisect_row(&repo.id, &rec, version))
            .await?;
        tx.emit_event(Self::event("bisect.finished", repo, &rec, None))
            .await?;
        if state == BisectState::Flaky {
            tx.emit_event(Self::event("bisect.flaky", repo, &rec, None))
                .await?;
        }
        tx.audit(
            AuditDraft::new(
                match state {
                    BisectState::Cancelled => "bisect.cancel",
                    BisectState::Flaky => "bisect.flaky",
                    _ => "bisect.inconclusive",
                },
                &repo.org_id,
            )
            .repo_id(&repo.id)
            .target(SubjectKind::CheckRun, &rec.failing_check_run_id)
            .after(&serde_json::json!({ "bisect": b.id, "note": note }).to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(b)
    }

    async fn finish_found(
        &self,
        cx: &Cx,
        repo: &Repo,
        mut rec: BisectRecord,
        version: u64,
        index: usize,
    ) -> Result<Bisect> {
        let prober = self.prober();
        for p in rec.probes.iter_mut().filter(|p| p.result.is_empty()) {
            let _ = prober.cancel(cx, repo, &p.probe_id).await;
            p.result = Outcome::Skip.as_str().into();
        }
        let culprit = oid(&rec.candidates[index]);
        let later: Vec<ObjectId> = rec.candidates[index + 1..].iter().map(|c| oid(c)).collect();
        let git_ref = rec
            .bisect
            .as_ref()
            .map(|b| b.r#ref.clone())
            .unwrap_or_default();
        let cr = Box::pin(self.changes.landed_by(cx, repo, &git_ref, &culprit, &later)).await?;
        if let Some(cr) = &cr {
            rec.culprit_change_request_id = cr.id.clone();
            rec.culprit_change_request_number = cr.number;
            rec.culprit_author = cr.author.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        }
        let mut b = rec.bisect.clone().unwrap_or_default();
        b.first_bad = Some(culprit.clone());
        b.state = BisectState::Found as i32;
        b.tested = rec
            .probes
            .iter()
            .filter(|p| matches!(p.result.as_str(), "pass" | "fail") && p.source != "history")
            .map(|p| oid(&p.changeset))
            .collect();
        {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            b.finished_at = Some(ts(tx.now_ms()));
            rec.bisect = Some(b.clone());
            rec.note = match &cr {
                Some(c) => format!(
                    "first failing changeset {} landed by change request #{}",
                    hex(&culprit.raw),
                    c.number
                ),
                None => format!(
                    "first failing changeset {} was not landed through a change request; no revert opened",
                    hex(&culprit.raw)
                ),
            };
            tx.update(&BISECTS, &bisect_row(&repo.id, &rec, version))
                .await?;
            let subject = cr.as_ref().map(|c| SubjectRef {
                kind: SubjectKind::ChangeRequest as i32,
                repo: crate::ci::engine::repo_ref_of(&repo.id),
                id: c.number.to_string(),
            });
            tx.emit_event(Self::event("bisect.found", repo, &rec, subject))
                .await?;
            tx.audit(
                AuditDraft::new("bisect.found", &repo.org_id)
                    .repo_id(&repo.id)
                    .target(SubjectKind::CheckRun, &rec.failing_check_run_id)
                    .after(
                        &serde_json::json!({
                            "bisect": b.id, "first_bad": hex(&culprit.raw),
                            "change_request": rec.culprit_change_request_id,
                            "rounds": rec.rounds,
                        })
                        .to_string(),
                    ),
            )
            .await?;
            tx.commit().await?;
        }
        if let Some(cr) = cr
            && rec.open_revert
        {
            return Box::pin(self.revert(cx, repo, &b.id, &cr)).await;
        }
        Ok(b)
    }

    /// Opens the revert of the culprit's CR, notifies its author and, with `auto_revert`,
    /// lands it (or queues it for landing).
    async fn revert(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        cr: &bee_proto::v1::ChangeRequest,
    ) -> Result<Bisect> {
        let (mut rec, row) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::load(&mut tx, &repo.id, id).await?
        };
        let b = rec.bisect.clone().unwrap_or_default();
        let author = rec.culprit_author.clone();
        let culprit = b
            .first_bad
            .as_ref()
            .map(|c| hex(&c.raw))
            .unwrap_or_default();
        let link = if rec.failing_url.is_empty() {
            String::new()
        } else {
            format!("\n\nFailing run: {}", rec.failing_url)
        };
        let body = format!(
            "Reverts #{}.\n\nPost-land bisect {} found {} as the first changeset where `{}` fails on `{}` ({} probe rounds).{link}",
            cr.number, b.id, culprit, b.check_name, b.r#ref, rec.rounds
        );
        let sys = Cx::system();
        let mut notes = vec![rec.note.clone()];
        match Box::pin(self.changes.revert(&sys, repo, &cr.id, "", &body, false)).await {
            Ok(rv) => {
                rec.bisect = Some(Bisect {
                    revert_change_request_id: rv.id.clone(),
                    ..b.clone()
                });
                notes.push(format!("revert opened as #{}", rv.number));
                if !author.is_empty() && author != sys.actor_id {
                    if let Err(e) = Box::pin(self.changes.request_reviewers(
                        &sys,
                        repo,
                        &rv.id,
                        std::slice::from_ref(&author),
                        &[],
                    ))
                    .await
                    {
                        notes.push(format!(
                            "could not request review from {author}: {}",
                            e.message
                        ));
                    }
                    let comment = format!(
                        "@{author} post-land check `{}` on `{}` started failing with this change (bisect {}). Revert: #{}.{link}",
                        b.check_name, b.r#ref, b.id, rv.number
                    );
                    if let Err(e) =
                        Box::pin(self.changes.comment(&sys, repo, &cr.id, &comment, false)).await
                    {
                        notes.push(format!(
                            "could not comment on #{}: {}",
                            cr.number, e.message
                        ));
                    }
                }
                let settings = self.orgs.repo_settings(cx, &repo.id).await?;
                if settings.auto_revert {
                    // The revert targets the culprit's ref: the queue is required there by
                    // the repo setting or by an active merge-queue ruleset.
                    let queued = settings.merge_queue_required
                        || self
                            .changes
                            .queue_required(cx, repo, &rv.target_ref)
                            .await
                            .unwrap_or(true);
                    let landed = if queued {
                        match Box::pin(self.changes.enqueue(
                            &sys,
                            repo,
                            &rv.id,
                            LandMethod::Unspecified,
                            0,
                        ))
                        .await
                        {
                            Ok(_) => {
                                notes.push("revert queued in the merge queue".into());
                                false
                            }
                            Err(e) => {
                                notes.push(format!("revert could not be queued: {}", e.message));
                                false
                            }
                        }
                    } else {
                        match Box::pin(self.changes.land(
                            &sys,
                            repo,
                            &rv.id,
                            LandMethod::Unspecified,
                            "",
                            0,
                        ))
                        .await
                        {
                            Ok(_) => {
                                notes.push("revert landed (auto_revert)".into());
                                true
                            }
                            Err(e) if settings.allow_auto_merge => {
                                match Box::pin(self.changes.enable_auto_merge(
                                    &sys,
                                    repo,
                                    &rv.id,
                                    LandMethod::Unspecified,
                                    "",
                                ))
                                .await
                                {
                                    Ok(_) => notes.push(format!(
                                        "revert could not land yet ({}); auto-merge enabled",
                                        e.message
                                    )),
                                    Err(e2) => notes.push(format!(
                                        "revert could not land ({}) nor auto-merge ({})",
                                        e.message, e2.message
                                    )),
                                }
                                false
                            }
                            Err(e) => {
                                notes.push(format!("revert could not land: {}", e.message));
                                false
                            }
                        }
                    };
                    rec.revert_landed = landed;
                }
            }
            Err(e) => notes.push(format!("the revert could not be opened: {}", e.message)),
        }
        rec.note = notes.join("; ");
        let b = rec.bisect.clone().unwrap_or_default();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.update(&BISECTS, &bisect_row(&repo.id, &rec, row.version))
            .await?;
        if !b.revert_change_request_id.is_empty() {
            tx.emit_event(Self::event("bisect.reverted", repo, &rec, None))
                .await?;
            tx.audit(
                AuditDraft::new("bisect.revert", &repo.org_id)
                    .repo_id(&repo.id)
                    .target(SubjectKind::ChangeRequest, &b.revert_change_request_id)
                    .after(
                        &serde_json::json!({
                            "bisect": b.id, "reverts": cr.id, "landed": rec.revert_landed,
                        })
                        .to_string(),
                    ),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(b)
    }

    /// One session.
    pub async fn get(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Bisect> {
        Ok(self.session(cx, repo, id).await?.bisect)
    }

    /// One session in full (candidates, probes, rounds, notes).
    pub async fn session(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<BisectSession> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(session_of(&Self::load(&mut tx, &repo.id, id).await?.0))
    }

    /// A repo's sessions, newest first, optionally in one state.
    pub async fn list(&self, cx: &Cx, repo: &Repo, state: BisectState) -> Result<Vec<Bisect>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scan = Scan::scope(repo.id.clone(), 1000);
        if state != BisectState::Unspecified {
            scan = scan.key(Key::K1(state_key(state).into()));
        }
        let mut out: Vec<Bisect> = tx
            .scan_msgs::<BisectRecord>(&BISECTS, &scan)
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.bisect)
            .collect();
        out.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(out)
    }

    /// The changeset a manual session should test next (`None` once decided).
    pub async fn next_candidate(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Option<ObjectId>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load(&mut tx, &repo.id, id).await?;
        if rec.bisect.as_ref().map(state_of) != Some(BisectState::Running) {
            return Ok(None);
        }
        let (plan, pending) = plan_of(&rec);
        Ok(plan
            .next_round(1, &pending)
            .first()
            .map(|i| oid(&rec.candidates[*i])))
    }

    /// Marks a candidate of a manual session good (`Pass`), bad (`Fail`) or untestable
    /// (`Skip`), then advances it. Errors: INVALID_STATE (not running, or an automatic
    /// session), INVALID_ARGUMENT (`not_a_candidate`).
    pub async fn mark(
        &self,
        cx: &Cx,
        repo: &Repo,
        id: &str,
        changeset: &ObjectId,
        outcome: Outcome,
    ) -> Result<Bisect> {
        {
            let lock = self.lock(id);
            let _g = lock.lock().await;
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let (mut rec, row) = Self::load(&mut tx, &repo.id, id).await?;
            let b = rec.bisect.clone().unwrap_or_default();
            if state_of(&b) != BisectState::Running {
                return Err(ForgeError::invalid_state(format!(
                    "bisect {id} has finished"
                )));
            }
            if rec.mode != "manual" {
                return Err(ForgeError::invalid_state(format!(
                    "bisect {id} is driven by CI probes; marks apply to manual sessions"
                )));
            }
            let want = hex(&changeset.raw);
            let Some(index) = rec.candidates.iter().position(|c| *c == want) else {
                return Err(ForgeError::invalid(
                    "not_a_candidate",
                    format!("{want} is not between the good and bad changesets"),
                ));
            };
            rec.probes.push(ProbeRecord {
                index: index as u32,
                changeset: want.clone(),
                probe_id: String::new(),
                result: outcome.as_str().into(),
                round: 0,
                source: cx.actor_id.clone(),
            });
            tx.update(&BISECTS, &bisect_row(&repo.id, &rec, row.version))
                .await?;
            tx.audit(
                AuditDraft::new("bisect.mark", &repo.org_id)
                    .repo_id(&repo.id)
                    .target_id(id)
                    .after(
                        &serde_json::json!({ "changeset": want, "outcome": outcome.as_str() })
                            .to_string(),
                    ),
            )
            .await?;
            tx.commit().await?;
        }
        Box::pin(self.step(cx, repo, id)).await
    }

    /// Cancels a running session and its probes.
    pub async fn cancel(&self, cx: &Cx, repo: &Repo, id: &str) -> Result<Bisect> {
        let lock = self.lock(id);
        let _g = lock.lock().await;
        let (rec, row) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::load(&mut tx, &repo.id, id).await?
        };
        let b = rec.bisect.clone().unwrap_or_default();
        if state_of(&b) != BisectState::Running {
            return Err(ForgeError::invalid_state(format!(
                "bisect {id} has finished"
            )));
        }
        let why = format!("cancelled by {}", cx.actor_id);
        self.finish(cx, repo, rec, row.version, BisectState::Cancelled, &why)
            .await
    }

    /// Advances every running session whose probes include `probe_id` (a finished run).
    pub async fn on_probe_finished(&self, cx: &Cx, repo: &Repo, probe_id: &str) -> Result<()> {
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan_msgs::<BisectRecord>(
                &BISECTS,
                &Scan::scope(repo.id.clone(), 1000).key(Key::K1("running".into())),
            )
            .await?
        };
        for (r, row) in rows {
            if r.probes
                .iter()
                .any(|p| p.result.is_empty() && p.probe_id == probe_id)
            {
                Box::pin(self.step(cx, repo, &row.id)).await?;
            }
        }
        Ok(())
    }

    /// Advances every running session (the maintenance tick).
    pub async fn tick(&self, cx: &Cx) -> Result<()> {
        let rows = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            tx.scan(
                &BISECTS,
                &Scan {
                    key: Some(Key::K1("running".into())),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?
        };
        for row in rows {
            let Ok(repo) = self.orgs.repo_any(cx, &row.scope).await else {
                continue;
            };
            let _ = Box::pin(self.step(&Cx::system(), &repo, &row.id)).await;
        }
        Ok(())
    }
}

/// Starts bisects on failed post-land checks and advances them when probe runs finish.
pub struct BisectListener {
    bisector: Arc<Bisector>,
}

impl BisectListener {
    /// A listener for `bisector`.
    pub fn new(bisector: Arc<Bisector>) -> BisectListener {
        BisectListener { bisector }
    }
}

#[async_trait]
impl EventListener for BisectListener {
    async fn on_event(&self, ev: &Event) {
        if ev.repo_id.is_empty() {
            return;
        }
        let interesting = match (ev.kind.as_str(), &ev.body) {
            ("check.updated", Some(event::Body::Check(run))) => run.post_land && failed(run),
            ("workflow_run.completed", _) => true,
            _ => false,
        };
        if !interesting {
            return;
        }
        let cx = Cx::system();
        let Ok(repo) = self.bisector.orgs.repo_any(&cx, &ev.repo_id).await else {
            return;
        };
        match (ev.kind.as_str(), &ev.body) {
            ("check.updated", Some(event::Body::Check(run))) => {
                let _ = Box::pin(self.bisector.on_post_land_failure(&cx, &repo, run)).await;
            }
            _ => {
                let id = ev
                    .subject
                    .as_ref()
                    .map(|s| s.id.clone())
                    .unwrap_or_default();
                if !id.is_empty() {
                    let _ = Box::pin(self.bisector.on_probe_finished(&cx, &repo, &id)).await;
                }
            }
        }
    }
}
