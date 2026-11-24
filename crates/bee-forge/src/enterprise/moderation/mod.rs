//! Moderation and community: blocks, reports, interaction limits, comment minimizing, locks,
//! moderators, first-time contributors, flood defenses and takedowns (PRODUCT §4.21). Serves
//! `ModerationService`.
//!
//! Owns the `moderation` surface.
//!
//! # Enforcement
//!
//! [`Moderation::check_write`] is the single enforcement point. The server calls it at every
//! write path (comment, reaction, issue, change request, review, assignment, mention, fork)
//! after authorization and before the write. Checks run in a fixed order and the first
//! failure wins:
//!
//! 1. **takedown**: the repo or the comment is taken down: `PERMISSION_DENIED` / `taken_down`;
//! 2. **block**: any principal of the delegation chain is blocked in the org, by the repo
//!    owner or by the author of the conversation: `BLOCKED`;
//! 3. **interaction limit**: repo, org and user limits in force: `INTERACTION_LIMITED`;
//! 4. **lock**: the conversation is locked: `LOCKED` (collaborators, maintainers and moderators
//!    may still write);
//! 5. **flood defenses**: throttle, open-CR caps, reputation and hourly creation rates:
//!    `RESOURCE_EXHAUSTED` with `retry_after_ms`.
//!
//! What remains is the first-time contributor handling, a decision rather than an error
//! ([`WriteDecision`]). Every counter and window is per repo, so a flood against one repo
//! never touches another.
//!
//! # Read set
//!
//! One check reads only by key, never by scan: 1-2 takedown marks, up to `3 x chain` block
//! rows, up to 3 limit rows, 1 lock row, 1 flood-limits row, and for CRs and issues 2
//! first-time settings rows. The flood stage adds the actor's throttle row, up to 2 counters
//! and one bounded window scan (at most the configured hourly limit), and only runs when the
//! repo has flood limits. Collaborator and moderator standing (the orgs surface) is read
//! lazily, only when a limit, lock or flood exemption makes it matter, and outside any
//! moderation transaction.
//!
//! # Other hooks
//!
//! The changes module calls [`Moderation::note_open`] and [`Moderation::note_closed`] around
//! the CR lifecycle, the work module calls [`Moderation::record_comment_edit`] when a comment
//! changes and [`Moderation::decorate_comment`] when it returns one, and anything that decides
//! the fate of a contribution calls [`Moderation::record_outcome`].

pub mod blocks;
pub mod check;
pub mod codes;
pub mod comments;
pub mod contributors;
pub mod flood;
pub mod limits;
pub mod moderators;
pub mod reports;
pub mod service;
pub mod tables;
pub mod takedowns;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_proto::v1::{
    AuditEntry, Comment, Event, ModerationFloodLimits, Principal, RepoRef, SubjectKind, SubjectRef,
    repo_ref,
};

use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::enterprise::common::TeamDirectory;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::module::{EventKind, ForgeModule};
use crate::store::{Db, OutboxRecord, Row, Surface, Tx};

pub use self::check::{FloodDecision, WriteCheck, WriteDecision, WriteKind};
pub use self::flood::Outcome;
pub use self::service::ModerationService;

/// The moderation surface.
pub const SURFACE: Surface = Surface("moderation");

/// Age below which an account counts as new (interaction limits, run approval).
pub const NEW_ACCOUNT_MS: u64 = 24 * 3600 * 1000;
/// The sliding window of the hourly creation limits.
pub const WINDOW_MS: u64 = 3600 * 1000;
/// Longest interaction limit and block duration accepted.
pub const MAX_DURATION_SECS: u64 = 15_552_000;

/// What the moderation module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "moderation",
    surfaces: &[SURFACE],
    migrations: &[&tables::M1],
    event_kinds: &[
        EventKind::gh(
            "moderation.blocked",
            "A user was blocked",
            "org_block",
            "blocked",
        ),
        EventKind::gh(
            "moderation.unblocked",
            "A user was unblocked or a block expired",
            "org_block",
            "unblocked",
        ),
        EventKind::new("moderation.reported", "Content was reported"),
        EventKind::new(
            "moderation.report_resolved",
            "A content report was resolved",
        ),
        EventKind::new("moderation.limit_set", "An interaction limit was set"),
        EventKind::new(
            "moderation.limit_removed",
            "An interaction limit was removed or expired",
        ),
        EventKind::new("moderation.comment_minimized", "A comment was minimized"),
        EventKind::new(
            "moderation.comment_unminimized",
            "A minimized comment was shown again",
        ),
        EventKind::new(
            "moderation.conversation_locked",
            "A conversation was locked",
        ),
        EventKind::new(
            "moderation.conversation_unlocked",
            "A conversation was unlocked",
        ),
        EventKind::new("moderation.moderator_added", "A moderator was added"),
        EventKind::new("moderation.moderator_removed", "A moderator was removed"),
        EventKind::new(
            "moderation.first_time_settings_set",
            "First-time contributor handling changed",
        ),
        EventKind::new("moderation.flood_limits_set", "Flood limits changed"),
        EventKind::new(
            "moderation.principal_throttled",
            "A principal was throttled by flood limits",
        ),
        EventKind::new(
            "moderation.throttle_released",
            "A throttle was lifted before it expired",
        ),
        EventKind::new(
            "moderation.reputation_changed",
            "A principal's reputation tally changed",
        ),
        EventKind::new("moderation.takedown_created", "A takedown was recorded"),
        EventKind::new("moderation.takedown_reinstated", "A takedown was reversed"),
    ],
    audit_prefixes: &["moderation."],
};

/// Facts about an account that interaction limits and first-time handling need. The user
/// service implements it; [`InMemoryStanding`] serves tests and `--dev`.
#[async_trait]
pub trait StandingProvider: Send + Sync {
    /// When the account was created (ms since the epoch), if known.
    async fn account_created_ms(&self, principal_id: &str) -> Option<u64>;
    /// Whether the principal has a prior contribution (a landed CR, an accepted change) in
    /// the repo.
    async fn has_contribution(&self, repo_id: &str, principal_id: &str) -> bool;
}

/// An in-memory [`StandingProvider`].
#[derive(Default)]
pub struct InMemoryStanding {
    created: RwLock<BTreeMap<String, u64>>,
    contributions: RwLock<BTreeSet<(String, String)>>,
}

impl InMemoryStanding {
    /// An empty provider: every account is of unknown age and has no contributions.
    pub fn new() -> InMemoryStanding {
        InMemoryStanding::default()
    }

    /// Records when an account was created.
    pub fn set_created(&self, principal_id: &str, created_ms: u64) {
        if let Ok(mut m) = self.created.write() {
            m.insert(principal_id.to_string(), created_ms);
        }
    }

    /// Records a prior contribution of a principal in a repo.
    pub fn add_contribution(&self, repo_id: &str, principal_id: &str) {
        if let Ok(mut s) = self.contributions.write() {
            s.insert((repo_id.to_string(), principal_id.to_string()));
        }
    }
}

#[async_trait]
impl StandingProvider for InMemoryStanding {
    async fn account_created_ms(&self, principal_id: &str) -> Option<u64> {
        self.created.read().ok()?.get(principal_id).copied()
    }

    async fn has_contribution(&self, repo_id: &str, principal_id: &str) -> bool {
        self.contributions
            .read()
            .map(|s| s.contains(&(repo_id.to_string(), principal_id.to_string())))
            .unwrap_or(false)
    }
}

/// Where [`CommentSource::comments_by_author`] looks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommentScope {
    /// Every repo of an org (the org id).
    Org(String),
    /// Content in the namespace of one user (the user id).
    Personal(String),
}

/// Reads comments from the work module, which owns them.
#[async_trait]
pub trait CommentSource: Send + Sync {
    /// One comment by id.
    async fn comment(&self, cx: &Cx, id: &str) -> Result<Option<Comment>>;
    /// The comments an author wrote in a scope, oldest first.
    async fn comments_by_author(
        &self,
        cx: &Cx,
        scope: &CommentScope,
        author_id: &str,
    ) -> Result<Vec<Comment>>;
}

/// An in-memory [`CommentSource`].
#[derive(Default)]
pub struct InMemoryComments {
    rows: RwLock<BTreeMap<String, (String, String, Comment)>>,
}

impl InMemoryComments {
    /// An empty source.
    pub fn new() -> InMemoryComments {
        InMemoryComments::default()
    }

    /// Adds a comment written in an org's repo; `owner_id` is the user namespace it belongs
    /// to (empty for none).
    pub fn insert(&self, org_id: &str, owner_id: &str, comment: Comment) {
        if let Ok(mut m) = self.rows.write() {
            m.insert(
                comment.id.clone(),
                (org_id.to_string(), owner_id.to_string(), comment),
            );
        }
    }
}

#[async_trait]
impl CommentSource for InMemoryComments {
    async fn comment(&self, _cx: &Cx, id: &str) -> Result<Option<Comment>> {
        Ok(self
            .rows
            .read()
            .map_err(|_| ForgeError::internal("comment source poisoned"))?
            .get(id)
            .map(|(_, _, c)| c.clone()))
    }

    async fn comments_by_author(
        &self,
        _cx: &Cx,
        scope: &CommentScope,
        author_id: &str,
    ) -> Result<Vec<Comment>> {
        let rows = self
            .rows
            .read()
            .map_err(|_| ForgeError::internal("comment source poisoned"))?;
        Ok(rows
            .values()
            .filter(|(org, owner, c)| {
                c.author.as_ref().is_some_and(|a| a.id == author_id)
                    && match scope {
                        CommentScope::Org(o) => org == o,
                        CommentScope::Personal(u) => owner == u,
                    }
            })
            .map(|(_, _, c)| c.clone())
            .collect())
    }
}

/// Moderation settings.
#[derive(Clone)]
pub struct ModerationConfig {
    /// Principal ids of site administrators.
    pub site_admins: BTreeSet<String>,
    /// Account age and contribution facts.
    pub standing: Arc<dyn StandingProvider>,
    /// Team membership (for team moderators); `None` serves principal moderators only.
    pub teams: Option<Arc<dyn TeamDirectory>>,
    /// Comment lookups (minimize, hide on block); `None` makes those RPCs unavailable.
    pub comments: Option<Arc<dyn CommentSource>>,
    /// Flood limits of repos that have no limits of their own; `None` = no limits.
    pub default_flood: Option<ModerationFloodLimits>,
}

impl ModerationConfig {
    /// A config with no site admins, teams or comment source.
    pub fn new(standing: Arc<dyn StandingProvider>) -> ModerationConfig {
        ModerationConfig {
            site_admins: BTreeSet::new(),
            standing,
            teams: None,
            comments: None,
            default_flood: None,
        }
    }

    /// Sets the site administrators.
    pub fn with_site_admins<I, S>(mut self, ids: I) -> ModerationConfig
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.site_admins = ids.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the team directory.
    pub fn with_teams(mut self, teams: Arc<dyn TeamDirectory>) -> ModerationConfig {
        self.teams = Some(teams);
        self
    }

    /// Sets the comment source.
    pub fn with_comments(mut self, comments: Arc<dyn CommentSource>) -> ModerationConfig {
        self.comments = Some(comments);
        self
    }

    /// Sets the flood limits repos inherit until they set their own.
    pub fn with_default_flood(mut self, limits: ModerationFloodLimits) -> ModerationConfig {
        self.default_flood = Some(limits);
        self
    }
}

/// The moderation domain over the forge.
pub struct Moderation {
    pub(crate) forge: Arc<Forge>,
    pub(crate) db: Db,
    pub(crate) cfg: ModerationConfig,
}

impl Moderation {
    /// Moderation over `forge` with `cfg`.
    pub fn new(forge: Arc<Forge>, cfg: ModerationConfig) -> Arc<Moderation> {
        Arc::new(Moderation {
            db: forge.db.clone(),
            forge,
            cfg,
        })
    }

    /// The forge.
    pub fn forge(&self) -> &Arc<Forge> {
        &self.forge
    }

    /// The settings.
    pub fn config(&self) -> &ModerationConfig {
        &self.cfg
    }

    /// Whether every principal of the call's delegation chain is a site administrator.
    pub fn is_site_admin(&self, cx: &Cx) -> bool {
        !cx.chain.is_empty() && cx.chain.iter().all(|p| self.cfg.site_admins.contains(p))
    }

    /// The principal message for an id (from the directory when known).
    pub(crate) async fn principal(&self, id: &str) -> Principal {
        super::common::principal_of(&self.forge, id).await
    }
}

/// One state change to record: the audit entry and the event that go with it.
pub(crate) struct Change {
    /// Event kind (registered in [`MODULE`]).
    pub kind: &'static str,
    /// Audit action (`moderation.<verb>`).
    pub action: &'static str,
    /// Org whose log records it (empty for the system log).
    pub org: String,
    /// Repo, when the change is repo-scoped.
    pub repo: String,
    /// The object acted on.
    pub subject: Option<SubjectRef>,
    /// State before, as JSON.
    pub before: Option<serde_json::Value>,
    /// State after, as JSON; also the event payload.
    pub after: serde_json::Value,
}

/// Records the event and the audit entry of a change in the transaction; returns the audit
/// entry id.
pub(crate) async fn record(tx: &mut Tx, c: Change) -> Result<String> {
    let cx = tx.cx().clone();
    let now = tx.now_ms();
    let mut draft = AuditDraft::new(c.action, &c.org).after(&c.after.to_string());
    if !c.repo.is_empty() {
        draft = draft.repo_id(&c.repo);
    }
    if let Some(s) = &c.subject {
        let kind = SubjectKind::try_from(s.kind).unwrap_or(SubjectKind::Unspecified);
        draft = draft.target(kind, &s.id);
    }
    if let Some(b) = &c.before {
        draft = draft.before(&b.to_string());
    }
    let audit_id = tx.new_id()?;
    let entry: AuditEntry = draft.into_entry(&cx, now, audit_id.clone());
    let outbox_id = tx.new_id()?;
    tx.emit_event(Event {
        kind: c.kind.into(),
        repo_id: c.repo.clone(),
        org_id: c.org.clone(),
        payload_json: c.after.to_string(),
        subject: c.subject.clone(),
        ..Default::default()
    })
    .await?;
    tx.raw()
        .outbox_push(
            &outbox_id,
            &OutboxRecord {
                event: None,
                audit: Some(entry),
            },
        )
        .await?;
    Ok(audit_id)
}

/// A subject naming `kind` and `id` inside a repo (by id).
pub(crate) fn subject_in(repo_id: &str, kind: SubjectKind, id: &str) -> SubjectRef {
    SubjectRef {
        kind: kind as i32,
        repo: (!repo_id.is_empty()).then(|| RepoRef {
            repo: Some(repo_ref::Repo::Id(repo_id.to_string())),
        }),
        id: id.to_string(),
    }
}

/// The repo id a subject names, when it names one by id.
pub(crate) fn subject_repo_id(s: &SubjectRef) -> Option<&str> {
    match s.repo.as_ref()?.repo.as_ref()? {
        repo_ref::Repo::Id(i) => Some(i.as_str()),
        repo_ref::Repo::FullName(_) => None,
    }
}

/// Stable key of a subject inside its repo: `<kind number>:<id>`.
pub(crate) fn subject_key(s: &SubjectRef) -> String {
    format!("{}:{}", s.kind, s.id)
}

/// `true` when the subject is a conversation that can be locked.
pub(crate) fn is_conversation(kind: i32) -> bool {
    matches!(
        SubjectKind::try_from(kind),
        Ok(SubjectKind::Issue | SubjectKind::ChangeRequest | SubjectKind::Discussion)
    )
}

/// One page of a table scope with a filter applied after the key filter: scans in batches
/// until `limit` rows pass `keep` or the scope ends. The token is the id of the last row
/// examined (empty at the end).
pub(crate) async fn filtered_page<M: prost::Message + Default>(
    tx: &mut Tx,
    t: &crate::store::TableDef,
    scope: &str,
    key: Option<crate::store::Key>,
    after: &str,
    limit: usize,
    keep: impl Fn(&M, &Row) -> bool,
) -> Result<(Vec<M>, String)> {
    use crate::enterprise::common::{MAX_LIST, page_cursor};
    const BATCH: usize = 200;
    let limit = limit.clamp(1, MAX_LIST);
    let mut out = Vec::new();
    let mut cursor = after.to_string();
    // Bounded: at most 50 batches, so one call reads at most 10,000 rows.
    for _ in 0..50 {
        let mut q = crate::store::Scan::scope(scope, BATCH).after(page_cursor(scope, &cursor));
        if let Some(k) = key.clone() {
            q = q.key(k);
        }
        let rows = tx.scan_msgs::<M>(t, &q).await?;
        let exhausted = rows.len() < BATCH;
        let n = rows.len();
        for (i, (m, row)) in rows.into_iter().enumerate() {
            cursor = row.id.clone();
            if keep(&m, &row) {
                out.push(m);
                if out.len() == limit {
                    let end = exhausted && i + 1 == n;
                    return Ok((out, if end { String::new() } else { cursor }));
                }
            }
        }
        if exhausted {
            return Ok((out, String::new()));
        }
    }
    Ok((out, cursor))
}
