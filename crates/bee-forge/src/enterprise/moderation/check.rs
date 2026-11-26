//! The enforcement point: [`Moderation::check_write`].
//!
//! See the module docs of [`super`] for the order of checks and the read set. A check never
//! fails for a reason that is not a SPEC §12.2 reason: takedowns are `PERMISSION_DENIED` with
//! subcode `taken_down`, blocks `BLOCKED`, interaction limits `INTERACTION_LIMITED`, locks
//! `LOCKED`, flood defenses `RESOURCE_EXHAUSTED` with `retry_after_ms`.
//!
//! [`Moderation::evaluate`] answers with a [`WriteDecision`] and never turns a policy refusal
//! into an error (`allow = false` with the refusal in `denial`); `check_write` is `evaluate`
//! followed by returning the denial as the error. Admitted writes of the kinds that have an
//! hourly creation limit (comments, issues, CRs) are recorded in the sliding window as part of
//! the check, unless the check is a dry run ([`WriteCheck::dry_run`]).

use bee_proto::v1::{
    ErrorReason, InteractionLimit, InteractionLimitLevel, ModerationFirstTimeContributorSettings,
    ModerationFloodLimits, ModerationLock, ModerationRunApproval, PrincipalKind, RepoRole,
    SubjectKind, SubjectRef,
};

use super::blocks::{BlockTarget, live_block};
use super::contributors::{self, default_settings};
use super::flood::{
    CAP_RETRY_MS, NewThrottle, counter, put_throttle, reputation_score, tally, window_scope,
};
use super::limits::{LimitTarget, live_limit};
use super::tables::{ACTIVITY, COUNTERS, FLOOD, LOCKS, THROTTLES};
use super::takedowns::{mark_key, marked};
use super::{Moderation, NEW_ACCOUNT_MS, SURFACE, WINDOW_MS, is_conversation, subject_key};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::orgs::role_rank;
use crate::store::{Row, Scan};

/// What a write does, for the checks that depend on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    /// A comment on an issue, CR, discussion or commit.
    Comment,
    /// A reaction.
    Reaction,
    /// Opening an issue.
    Issue,
    /// Opening a change request.
    ChangeRequest,
    /// A review of a change request.
    Review,
    /// Assigning someone (the assignee is the checked principal).
    Assignment,
    /// A mention that notifies someone.
    Mention,
    /// Forking a repo.
    Fork,
    /// Any other write.
    Other,
}

impl WriteKind {
    /// The name of the hourly limit bucket, for kinds that have one.
    fn bucket(self) -> Option<&'static str> {
        match self {
            WriteKind::Comment => Some("comment"),
            WriteKind::Issue => Some("issue"),
            WriteKind::ChangeRequest => Some("cr"),
            _ => None,
        }
    }
}

/// One write to check.
#[derive(Clone, Debug)]
pub struct WriteCheck {
    /// The call: actor, delegation chain (actor first, the owner last) and deadline.
    pub cx: Cx,
    /// The repo written to.
    pub repo_id: String,
    /// The repo's org.
    pub org_id: String,
    /// What kind of write.
    pub kind: WriteKind,
    /// The conversation or object written to (for lock checks and comment takedowns).
    pub subject: Option<SubjectRef>,
    /// Whether the actor has collaborator standing; `None` computes it from the repo roles.
    pub is_collaborator: Option<bool>,
    /// The user who started the conversation (their personal block list applies).
    pub subject_author: Option<String>,
    /// The user who owns the repo's namespace, if it is a user's (their personal block list
    /// and user-scope limits apply).
    pub repo_owner: Option<String>,
    /// Check only: do not record the write in the creation-rate window or create throttles.
    pub dry_run: bool,
}

impl WriteCheck {
    /// A check of `kind` by the caller of `cx` in a repo.
    pub fn new(cx: &Cx, repo_id: &str, org_id: &str, kind: WriteKind) -> WriteCheck {
        WriteCheck {
            cx: cx.clone(),
            repo_id: repo_id.to_string(),
            org_id: org_id.to_string(),
            kind,
            subject: None,
            is_collaborator: None,
            subject_author: None,
            repo_owner: None,
            dry_run: false,
        }
    }

    /// Names the conversation or object written to.
    pub fn subject(mut self, s: SubjectRef) -> WriteCheck {
        self.subject = Some(s);
        self
    }

    /// States whether the actor has collaborator standing.
    pub fn collaborator(mut self, yes: bool) -> WriteCheck {
        self.is_collaborator = Some(yes);
        self
    }

    /// Names the user who started the conversation.
    pub fn subject_author(mut self, id: &str) -> WriteCheck {
        self.subject_author = Some(id.to_string());
        self
    }

    /// Names the user whose namespace the repo is in.
    pub fn repo_owner(mut self, id: &str) -> WriteCheck {
        self.repo_owner = Some(id.to_string());
        self
    }

    /// Makes the check side-effect free.
    pub fn dry_run(mut self) -> WriteCheck {
        self.dry_run = true;
        self
    }
}

/// The answer to a [`WriteCheck`].
#[derive(Clone, Debug, PartialEq)]
pub struct WriteDecision {
    /// Whether the write may proceed.
    pub allow: bool,
    /// Labels to attach to what the write creates (for example `first-time-contributor`).
    pub labels: Vec<String>,
    /// The CR waits for maintainer approval before review starts.
    pub hold: bool,
    /// Workflow runs of this CR need approval.
    pub require_run_approval: bool,
    /// Set when a flood throttle refused the write: when it ends (ms since the epoch).
    pub throttled_until: Option<u64>,
    /// The refusal, when `allow` is false.
    pub denial: Option<ForgeError>,
}

impl WriteDecision {
    fn allowed() -> WriteDecision {
        WriteDecision {
            allow: true,
            labels: Vec::new(),
            hold: false,
            require_run_approval: false,
            throttled_until: None,
            denial: None,
        }
    }

    fn denied(e: ForgeError) -> WriteDecision {
        WriteDecision {
            allow: false,
            denial: Some(e),
            ..WriteDecision::allowed()
        }
    }
}

/// What the flood defenses say about a write, for servers that enforce them separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FloodDecision {
    /// Whether the write may proceed.
    pub allowed: bool,
    /// The limit that refused it (empty when allowed).
    pub limit: String,
    /// When to retry (0 when allowed).
    pub retry_after_ms: u64,
    /// When the throttle ends, if one applies.
    pub throttled_until: Option<u64>,
}

/// Everything the first stage reads, by key, in one moderation transaction.
struct Facts {
    now: u64,
    taken_down: bool,
    block: Option<String>,
    limits: Vec<InteractionLimit>,
    lock: Option<ModerationLock>,
    flood: Option<ModerationFloodLimits>,
    first_time: Option<ModerationFirstTimeContributorSettings>,
}

/// Standing facts read from the orgs surface only when a check needs them.
struct Standing<'a> {
    m: &'a Moderation,
    c: &'a WriteCheck,
    /// Who is judged: the owner for agents, the actor otherwise.
    id: String,
    is_agent: bool,
    collab: Option<bool>,
    moderator: Option<bool>,
}

impl<'a> Standing<'a> {
    fn new(m: &'a Moderation, c: &'a WriteCheck) -> Standing<'a> {
        let is_agent = c.cx.actor_kind == PrincipalKind::Agent;
        Standing {
            m,
            c,
            id: if is_agent {
                c.cx.root().to_string()
            } else {
                c.cx.actor_id.clone()
            },
            is_agent,
            collab: None,
            moderator: None,
        }
    }

    async fn collaborator(&mut self) -> Result<bool> {
        if let Some(v) = self.c.is_collaborator {
            return Ok(v);
        }
        if let Some(v) = self.collab {
            return Ok(v);
        }
        let cx = &self.c.cx;
        let repo = self.m.forge.orgs.repo_any(cx, &self.c.repo_id).await?;
        let perm = self
            .m
            .forge
            .orgs
            .effective_role(cx, &repo, &self.id)
            .await?;
        let role = RepoRole::try_from(perm.role).unwrap_or(RepoRole::None);
        let v = role_rank(role) >= role_rank(RepoRole::Triage);
        self.collab = Some(v);
        Ok(v)
    }

    async fn moderator(&mut self) -> Result<bool> {
        if let Some(v) = self.moderator {
            return Ok(v);
        }
        let v = self
            .m
            .is_moderator(&self.c.cx, &self.c.org_id, &self.id)
            .await?;
        self.moderator = Some(v);
        Ok(v)
    }

    /// Collaborators, maintainers and moderators.
    async fn privileged(&mut self) -> Result<bool> {
        Ok(self.collaborator().await? || self.moderator().await?)
    }
}

impl Moderation {
    /// Checks a write; fails with the SPEC §12.2 reason of the first check that refuses it.
    /// On success the decision carries the first-time contributor handling to apply.
    pub async fn check_write(&self, c: &WriteCheck) -> Result<WriteDecision> {
        let d = self.evaluate(c).await?;
        match d.denial.clone() {
            Some(e) => Err(e),
            None => Ok(d),
        }
    }

    /// Like [`Moderation::check_write`] but a refusal is a decision (`allow = false`), not an
    /// error. Errors are store failures only.
    pub async fn evaluate(&self, c: &WriteCheck) -> Result<WriteDecision> {
        let facts = self.read_facts(c).await?;
        let now = facts.now;
        if facts.taken_down {
            return Ok(WriteDecision::denied(
                ForgeError::permission_denied("this content has been taken down")
                    .with_subcode("taken_down"),
            ));
        }
        if let Some(scope) = &facts.block {
            return Ok(WriteDecision::denied(
                ForgeError::new(ErrorReason::Blocked, format!("you are blocked ({scope})"))
                    .with_subcode("blocked"),
            ));
        }
        let mut standing = Standing::new(self, c);
        for l in &facts.limits {
            if let Some(e) = self.limit_denial(l, &mut standing, now).await? {
                return Ok(WriteDecision::denied(e));
            }
        }
        if let Some(lock) = &facts.lock
            && !standing.privileged().await?
        {
            let why = bee_proto::v1::ModerationLockReason::try_from(lock.reason)
                .map(|r| r.as_str_name().to_lowercase())
                .unwrap_or_default();
            return Ok(WriteDecision::denied(
                ForgeError::new(
                    ErrorReason::Locked,
                    format!("conversation is locked ({why})"),
                )
                .with_subcode("conversation_locked"),
            ));
        }
        if let (Some(fl), Some(bucket)) = (&facts.flood, c.kind.bucket())
            && let Some(f) = self.flood_stage(c, fl, bucket, &mut standing, now).await?
        {
            let mut d = WriteDecision::denied(
                ForgeError::new(
                    ErrorReason::ResourceExhausted,
                    format!("flood limit {} reached", f.limit),
                )
                .with_subcode(&f.limit)
                .with_retry_after(f.retry_after_ms),
            );
            d.throttled_until = f.throttled_until;
            return Ok(d);
        }
        self.first_time_decision(c, &facts, &mut standing, now)
            .await
    }

    /// The flood defenses' answer for a write, without recording anything. Servers that
    /// enforce flood limits at a different point than the other checks use it; it is also what
    /// [`Moderation::evaluate`] consults.
    pub async fn flood_decision(&self, c: &WriteCheck) -> Result<FloodDecision> {
        let mut probe = c.clone();
        probe.dry_run = true;
        let facts = self.read_facts(&probe).await?;
        let mut standing = Standing::new(self, &probe);
        if let (Some(fl), Some(bucket)) = (&facts.flood, probe.kind.bucket())
            && let Some(f) = self
                .flood_stage(&probe, fl, bucket, &mut standing, facts.now)
                .await?
        {
            return Ok(f);
        }
        Ok(FloodDecision {
            allowed: true,
            limit: String::new(),
            retry_after_ms: 0,
            throttled_until: None,
        })
    }

    /// First stage: every read the checks need, by key, in one transaction.
    async fn read_facts(&self, c: &WriteCheck) -> Result<Facts> {
        let cx = &c.cx;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let mut facts = Facts {
            now,
            taken_down: false,
            block: None,
            limits: Vec::new(),
            lock: None,
            flood: None,
            first_time: None,
        };
        // 1. takedown
        if marked(&mut tx, &format!("repo:{}", c.repo_id)).await? {
            facts.taken_down = true;
        } else if let Some(s) = &c.subject
            && let Some(k) = mark_key(s.kind, &c.repo_id, &s.id)
            && marked(&mut tx, &k).await?
        {
            facts.taken_down = true;
        }
        // 2. blocks: every principal of the chain against the org, the repo owner's and the
        // conversation author's personal lists.
        let mut scopes = vec![BlockTarget::Org(c.org_id.clone())];
        for u in [&c.repo_owner, &c.subject_author].into_iter().flatten() {
            let t = BlockTarget::Personal(u.clone());
            if !scopes.contains(&t) {
                scopes.push(t);
            }
        }
        'blocks: for s in &scopes {
            for p in &cx.chain {
                if live_block(&mut tx, &s.key(), p, now).await?.is_some() {
                    facts.block = Some(match s {
                        BlockTarget::Org(_) => format!("org block of {p}"),
                        BlockTarget::Personal(_) => format!("personal block of {p}"),
                    });
                    break 'blocks;
                }
            }
        }
        // 3. interaction limits
        let mut targets = vec![
            LimitTarget::Repo(c.repo_id.clone()),
            LimitTarget::Org(c.org_id.clone()),
        ];
        if let Some(u) = &c.repo_owner {
            targets.push(LimitTarget::User(u.clone()));
        }
        for t in targets {
            if let Some(l) = live_limit(&mut tx, &t.key(), now).await? {
                facts.limits.push(l);
            }
        }
        // 4. lock
        if let Some(s) = &c.subject
            && is_conversation(s.kind)
        {
            facts.lock = tx
                .get_msg::<ModerationLock>(&LOCKS, &c.repo_id, &subject_key(s))
                .await?
                .map(|(l, _)| l);
        }
        // 5. flood limits (only for the kinds that have them)
        if c.kind.bucket().is_some() {
            facts.flood = match tx
                .get_msg::<ModerationFloodLimits>(&FLOOD, &c.repo_id, "limits")
                .await?
            {
                Some((l, _)) => Some(l),
                None => self.cfg.default_flood.clone(),
            };
        }
        // first-time contributor settings
        if matches!(c.kind, WriteKind::Issue | WriteKind::ChangeRequest) {
            facts.first_time = Some(contributors::effective(&mut tx, &c.repo_id, &c.org_id).await?);
        }
        Ok(facts)
    }

    async fn limit_denial(
        &self,
        l: &InteractionLimit,
        st: &mut Standing<'_>,
        now: u64,
    ) -> Result<Option<ForgeError>> {
        if st.is_agent && !l.applies_to_agents {
            return Ok(None);
        }
        let level =
            InteractionLimitLevel::try_from(l.level).unwrap_or(InteractionLimitLevel::Unspecified);
        if level == InteractionLimitLevel::Unspecified || st.privileged().await? {
            return Ok(None);
        }
        let ok = match level {
            InteractionLimitLevel::ExistingUsers => self
                .cfg
                .standing
                .account_created_ms(&st.id)
                .await
                .is_some_and(|t| now.saturating_sub(t) >= NEW_ACCOUNT_MS),
            InteractionLimitLevel::ContributorsOnly => {
                self.cfg
                    .standing
                    .has_contribution(&st.c.repo_id, &st.id)
                    .await
            }
            InteractionLimitLevel::CollaboratorsOnly | InteractionLimitLevel::Unspecified => false,
        };
        if ok {
            return Ok(None);
        }
        let expires = l.expires_at.as_ref().map(ts_ms).unwrap_or(now);
        let name = level
            .as_str_name()
            .trim_start_matches("INTERACTION_LIMIT_LEVEL_")
            .to_lowercase();
        Ok(Some(
            ForgeError::new(
                ErrorReason::InteractionLimited,
                format!(
                    "interactions are limited to {} until {}",
                    name.replace('_', " "),
                    expires
                ),
            )
            .with_subcode(&name)
            .with_retry_after(expires.saturating_sub(now)),
        ))
    }

    /// Flood stage: throttle, open-CR caps, reputation, hourly rate. `None` = admitted.
    async fn flood_stage(
        &self,
        c: &WriteCheck,
        fl: &ModerationFloodLimits,
        bucket: &str,
        st: &mut Standing<'_>,
        now: u64,
    ) -> Result<Option<FloodDecision>> {
        if fl.exempt_collaborators && st.privileged().await? {
            return Ok(None);
        }
        let cx = &c.cx;
        let actor = cx.actor_id.as_str();
        let owner = cx.root();
        let rate_max = match bucket {
            "cr" => fl.max_new_change_requests_per_hour_per_owner,
            "comment" => fl.max_new_comments_per_hour_per_owner,
            _ => fl.max_new_issues_per_hour_per_owner,
        } as usize;
        let refuse = |limit: &str, retry: u64, until: Option<u64>| {
            Ok(Some(FloodDecision {
                allowed: false,
                limit: limit.to_string(),
                retry_after_ms: retry.max(1),
                throttled_until: until,
            }))
        };
        let dur_ms = fl.throttle_duration_seconds.saturating_mul(1000);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        // Throttle already in force.
        if let Some(row) = tx.get(&THROTTLES, &c.repo_id, actor).await?
            && let Some(exp) = row.n1.filter(|e| *e as u64 > now)
        {
            return refuse("throttled", exp as u64 - now, Some(exp as u64));
        }
        // Open-CR caps.
        if bucket == "cr" {
            let caps = [
                (
                    fl.max_open_change_requests_per_principal,
                    format!("cr:p:{actor}"),
                    "max_open_change_requests_per_principal",
                ),
                (
                    fl.max_open_change_requests_per_owner,
                    format!("cr:o:{owner}"),
                    "max_open_change_requests_per_owner",
                ),
            ];
            for (cap, id, name) in caps {
                if cap > 0 && counter(&mut tx, &c.repo_id, &id).await? >= i64::from(cap) {
                    return refuse(name, CAP_RETRY_MS, None);
                }
            }
        }
        // Reputation (the lowest score of the chain).
        if fl.min_reputation > 0.0 {
            let mut lowest = f64::MAX;
            for p in &cx.chain {
                lowest = lowest.min(reputation_score(&tally(&mut tx, p).await?));
            }
            if lowest < fl.min_reputation {
                let retry = if dur_ms > 0 { dur_ms } else { CAP_RETRY_MS };
                let until = (dur_ms > 0).then_some(now + dur_ms);
                if let (Some(u), false) = (until, c.dry_run) {
                    put_throttle(
                        &mut tx,
                        NewThrottle {
                            repo_id: &c.repo_id,
                            org_id: &c.org_id,
                            principal: cx.principal(),
                            owner_id: owner,
                            limit: "min_reputation",
                            reputation: lowest,
                            recent_count: 0,
                            expires_ms: u,
                        },
                    )
                    .await?;
                    tx.commit().await?;
                }
                return refuse("min_reputation", retry, until);
            }
        }
        // Hourly creation rate over the sliding window.
        if rate_max > 0 {
            let limit_name = match bucket {
                "cr" => "max_new_change_requests_per_hour_per_owner",
                "comment" => "max_new_comments_per_hour_per_owner",
                _ => "max_new_issues_per_hour_per_owner",
            };
            if !c.dry_run {
                // Serializes admissions per owner and kind (the row stays locked until the
                // transaction ends), so concurrent writes cannot both take the last slot.
                tx.add_n1(&COUNTERS, &c.repo_id, &format!("adm:{owner}:{bucket}"), 1)
                    .await?;
            }
            let scope = window_scope(&c.repo_id, owner, bucket);
            let rows = tx
                .scan(
                    &ACTIVITY,
                    &Scan::scope(scope.clone(), rate_max + 64)
                        .by_n1()
                        .n1_min((now + 1).saturating_sub(WINDOW_MS) as i64),
                )
                .await?;
            if rows.len() >= rate_max {
                let idx = rows.len() - rate_max;
                let free_at = rows[idx].n1.unwrap_or(now as i64) as u64 + WINDOW_MS;
                let window_retry = free_at.saturating_sub(now);
                if dur_ms > 0 {
                    let expires = now + dur_ms.max(window_retry);
                    if !c.dry_run {
                        put_throttle(
                            &mut tx,
                            NewThrottle {
                                repo_id: &c.repo_id,
                                org_id: &c.org_id,
                                principal: cx.principal(),
                                owner_id: owner,
                                limit: limit_name,
                                reputation: 0.0,
                                recent_count: rows.len() as u32,
                                expires_ms: expires,
                            },
                        )
                        .await?;
                        tx.commit().await?;
                    }
                    return refuse(limit_name, expires - now, Some(expires));
                }
                if !c.dry_run {
                    tx.commit().await?;
                }
                return refuse(limit_name, window_retry, None);
            }
            if !c.dry_run {
                let id = tx.new_id()?;
                tx.insert(&ACTIVITY, &Row::new(scope, id).k1(actor).n1(now as i64))
                    .await?;
                tx.commit().await?;
            }
        }
        Ok(None)
    }

    /// Last stage: first-time contributor handling.
    async fn first_time_decision(
        &self,
        c: &WriteCheck,
        facts: &Facts,
        st: &mut Standing<'_>,
        now: u64,
    ) -> Result<WriteDecision> {
        let mut d = WriteDecision::allowed();
        if !matches!(c.kind, WriteKind::Issue | WriteKind::ChangeRequest) {
            return Ok(d);
        }
        let s = facts.first_time.clone().unwrap_or_else(default_settings);
        let run = ModerationRunApproval::try_from(s.run_approval)
            .unwrap_or(ModerationRunApproval::Unspecified);
        let standing = &self.cfg.standing;
        let own = standing.has_contribution(&c.repo_id, &c.cx.actor_id).await;
        let contributed = own
            || (st.is_agent
                && !s.treat_new_agents_as_first_time
                && standing.has_contribution(&c.repo_id, &st.id).await);
        let needs_standing = !contributed || run == ModerationRunApproval::AllOutside;
        let privileged = if needs_standing {
            st.privileged().await?
        } else {
            false
        };
        let first_time = !contributed && !privileged;
        if first_time {
            if s.label_first_contributions && !s.label.is_empty() {
                d.labels.push(s.label.clone());
            }
            if c.kind == WriteKind::ChangeRequest {
                d.hold = s.hold_change_requests;
            }
        }
        if c.kind == WriteKind::ChangeRequest {
            d.require_run_approval = match run {
                ModerationRunApproval::FirstTime => first_time,
                ModerationRunApproval::NewAccounts => {
                    first_time
                        && standing
                            .account_created_ms(&st.id)
                            .await
                            .is_none_or(|t| now.saturating_sub(t) < NEW_ACCOUNT_MS)
                }
                ModerationRunApproval::AllOutside => !privileged,
                ModerationRunApproval::Unspecified => false,
            };
        }
        Ok(d)
    }
}

/// `true` when `kind` names a conversation whose lock applies (helper for servers).
pub fn is_lockable(kind: SubjectKind) -> bool {
    is_conversation(kind as i32)
}
