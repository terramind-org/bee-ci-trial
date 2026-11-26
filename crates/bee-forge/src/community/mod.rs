//! Collaboration surfaces (PRODUCT §4.16): discussions, wikis, gists, releases,
//! notifications, user profiles, insights and sponsors. Serves `DiscussionService`,
//! `WikiService`, `GistService`, `ReleaseService`, `NotificationService`, `UserService`,
//! `InsightsService` and `SponsorService`; the contract is `docs/spec/forge-community.md`.
//!
//! # Layout
//!
//! | file | owns |
//! |---|---|
//! | [`discussions`], [`reactions`] | categories, threads, comments, answers, polls, upvotes, conversion; reactions on any subject |
//! | [`wiki`], [`gists`] | `<repo>.wiki` pages and gists over the [`ports::ContentVcs`] port |
//! | [`releases`] | tags, releases, assets, generated notes, source archive requests |
//! | [`notifications`], [`fanout`] | the inbox, subscriptions, watches, reminders; event fan-out and delivery queues |
//! | [`users`] | profiles, follows, stars, emails, settings, rename, export, delete |
//! | [`keys`] | GPG and SSH signing keys of accounts |
//! | [`activity`] | contribution graphs, activity overviews, the home feed, achievements (derived from events) |
//! | [`insights`], [`repofiles`] | pulse, contributors, traffic, network; languages, license, community files, citation |
//! | [`sponsors`] | profiles, tiers, sponsorships, sponsor-only access, `FUNDING.yml` |
//! | [`ports`] | what the server backs: VCS, tags, change requests, activity, issues, dependencies, URLs, payments |
//! | [`svc`] | the tonic services |
//!
//! # Surfaces
//!
//! Three failure domains, each with its own pool: `community` (discussions, wiki, gists,
//! releases, sponsors, traffic), `notifications` (the inbox and its queues, the busiest) and
//! `users` (profiles, social graph).
//!
//! [`Community`] wires the stores, ports and workers; the forge holds one.

pub mod activity;
pub mod backing;
pub mod common;
pub mod discussions;
pub mod fanout;
pub mod gists;
pub mod insights;
pub mod keys;
pub mod lang;
pub mod notifications;
pub mod ports;
pub mod reactions;
pub mod releases;
pub mod repofiles;
pub mod sigverify;
pub mod sponsors;
pub mod svc;
pub mod tz;
pub mod users;
pub mod wiki;
pub mod yaml;

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::RepoRole;

use crate::cx::Cx;
use crate::error::Result;
use crate::events::EventLog;
use crate::hooks::{PrincipalDirectory, RevisionResolver};
use crate::module::{EventKind, ForgeModule};
use crate::orgs::{Orgs, role_rank};
use crate::store::{Db, Migration, MigrationOp, Phase, Surface};

pub use notifications::NOTIFICATIONS;
pub use ports::Ports;
pub use users::USERS;

/// The community surface.
pub const SURFACE: Surface = Surface("community");

static M1: Migration = Migration {
    module: "community",
    version: 1,
    name: "create_community",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&reactions::REACTIONS),
        MigrationOp::CreateTable(&discussions::CATEGORIES),
        MigrationOp::CreateTable(&discussions::DISCUSSIONS),
        MigrationOp::CreateTable(&discussions::COMMENTS),
        MigrationOp::CreateTable(&discussions::VOTES),
        MigrationOp::CreateTable(&discussions::UPVOTES),
        MigrationOp::CreateTable(&discussions::COUNTERS),
        MigrationOp::CreateTable(&wiki::WIKIS),
        MigrationOp::CreateTable(&gists::GISTS),
        MigrationOp::CreateTable(&gists::STARS),
        MigrationOp::CreateTable(&gists::COMMENTS),
        MigrationOp::CreateTable(&releases::RELEASES),
        MigrationOp::CreateTable(&releases::ASSETS),
        MigrationOp::CreateTable(&releases::TOMBSTONES),
        MigrationOp::CreateTable(&releases::ARCHIVES),
        MigrationOp::CreateTable(&releases::ATTESTATIONS),
        MigrationOp::CreateTable(&insights::TRAFFIC),
        MigrationOp::CreateTable(&insights::VISITORS),
        MigrationOp::CreateTable(&sponsors::PROFILES),
        MigrationOp::CreateTable(&sponsors::TIERS),
        MigrationOp::CreateTable(&sponsors::SPONSORSHIPS),
        MigrationOp::CreateTable(&sponsors::PAYOUTS),
    ],
};

static M2: Migration = Migration {
    module: "community",
    version: 2,
    name: "create_notifications",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&notifications::THREADS),
        MigrationOp::CreateTable(&notifications::SUBS),
        MigrationOp::CreateTable(&notifications::WATCHES),
        MigrationOp::CreateTable(&notifications::SETTINGS),
        MigrationOp::CreateTable(&notifications::FEED),
        MigrationOp::CreateTable(&notifications::COUNTERS),
        MigrationOp::CreateTable(&notifications::TOKENS),
        MigrationOp::CreateTable(&notifications::REMINDERS),
        MigrationOp::CreateTable(&notifications::DELIVERIES),
        MigrationOp::CreateTable(&notifications::JOBS),
        MigrationOp::CreateTable(&notifications::CURSORS),
    ],
};

static M3: Migration = Migration {
    module: "community",
    version: 3,
    name: "create_users",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&users::PROFILES),
        MigrationOp::CreateTable(&users::LOGINS),
        MigrationOp::CreateTable(&users::EMAILS),
        MigrationOp::CreateTable(&users::EMAIL_TOKENS),
        MigrationOp::CreateTable(&users::PLACEHOLDERS),
        MigrationOp::CreateTable(&users::FOLLOWS),
        MigrationOp::CreateTable(&users::STARS),
        MigrationOp::CreateTable(&users::STAR_LISTS),
        MigrationOp::CreateTable(&users::STAR_LIST_ITEMS),
        MigrationOp::CreateTable(&users::COUNTERS),
        MigrationOp::CreateTable(&users::EXPORTS),
        MigrationOp::CreateTable(&users::SECURITY_EVENTS),
        MigrationOp::CreateTable(&users::AVATAR_UPLOADS),
    ],
};

static M4: Migration = Migration {
    module: "community",
    version: 4,
    name: "create_activity_and_signing_keys",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&activity::ACTIVITY),
        MigrationOp::CreateTable(&activity::CURSORS),
        MigrationOp::CreateTable(&keys::SIGNING_KEYS),
    ],
};

static M5: Migration = Migration {
    module: "community",
    version: 5,
    name: "create_repo_follows",
    phase: Phase::Expand,
    ops: &[MigrationOp::CreateTable(&users::REPO_FOLLOWS)],
};

const COMMENT_EVENTS: &[EventKind] = &[EventKind::new("comment.edited", "A comment was edited")];

const PARTS: [&[EventKind]; 11] = [
    reactions::EVENTS,
    discussions::EVENTS,
    wiki::EVENTS,
    gists::EVENTS,
    releases::EVENTS,
    notifications::EVENTS,
    users::EVENTS,
    insights::EVENTS,
    sponsors::EVENTS,
    COMMENT_EVENTS,
    &[],
];

const KIND_COUNT: usize = reactions::EVENTS.len()
    + discussions::EVENTS.len()
    + wiki::EVENTS.len()
    + gists::EVENTS.len()
    + releases::EVENTS.len()
    + notifications::EVENTS.len()
    + users::EVENTS.len()
    + insights::EVENTS.len()
    + sponsors::EVENTS.len()
    + COMMENT_EVENTS.len();

const KINDS: [EventKind; KIND_COUNT] = {
    let mut out = [EventKind::new("", ""); KIND_COUNT];
    let (mut n, mut p) = (0, 0);
    while p < PARTS.len() {
        let mut i = 0;
        while i < PARTS[p].len() {
            out[n] = PARTS[p][i];
            n += 1;
            i += 1;
        }
        p += 1;
    }
    out
};

/// What the community module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "community",
    surfaces: &[SURFACE, NOTIFICATIONS, USERS],
    migrations: &[&M1, &M2, &M3, &M4, &M5],
    event_kinds: &KINDS,
    audit_prefixes: &[
        "discussion.",
        "release.",
        "tag.",
        "wiki.",
        "gist.",
        "comment.",
        "reaction.",
        "notification.",
        "user.",
        "follow.",
        "star.",
        "star_list.",
        "sponsor.",
        "insights.",
    ],
};

/// Settings of the community module.
#[derive(Clone, Debug)]
pub struct CommunityConfig {
    /// Base URL of permalinks and clone URLs.
    pub base_url: String,
    /// Notification fan-out and delivery.
    pub notify: notifications::NotifyConfig,
    /// Users (noreply domain).
    pub users: users::UsersConfig,
}

impl Default for CommunityConfig {
    fn default() -> CommunityConfig {
        CommunityConfig {
            base_url: "https://beehive.invalid".into(),
            notify: notifications::NotifyConfig::default(),
            users: users::UsersConfig::default(),
        }
    }
}

/// Read access for the notification fan-out, over the orgs module.
pub struct OrgsAccess(pub Arc<Orgs>);

#[async_trait]
impl notifications::ReadAccess for OrgsAccess {
    async fn can_read(&self, repo_id: &str, org_id: &str, principal_id: &str) -> bool {
        let cx = Cx::system();
        if repo_id.is_empty() {
            return matches!(self.0.member(&cx, org_id, principal_id).await, Ok(Some(_)));
        }
        let Ok(repo) = self.0.repo_any(&cx, repo_id).await else {
            return false;
        };
        if repo.deleted_at.is_some() {
            return false;
        }
        match self.0.effective_role(&cx, &repo, principal_id).await {
            Ok(p) => role_rank(RepoRole::try_from(p.role).unwrap_or(RepoRole::None)) > 0,
            Err(_) => false,
        }
    }

    async fn repo_name(&self, repo_id: &str) -> String {
        self.0
            .repo_any(&Cx::system(), repo_id)
            .await
            .map(|r| r.full_name)
            .unwrap_or_default()
    }
}

/// The community stores, ports and workers.
pub struct Community {
    /// Reactions.
    pub reactions: Arc<reactions::Reactions>,
    /// Discussions.
    pub discussions: Arc<discussions::Discussions>,
    /// Wikis.
    pub wiki: Arc<wiki::Wiki>,
    /// Gists.
    pub gists: Arc<gists::Gists>,
    /// Releases and tags.
    pub releases: Arc<releases::Releases>,
    /// Notifications.
    pub notifications: Arc<notifications::Notifications>,
    /// Users.
    pub users: Arc<users::Users>,
    /// Insights.
    pub insights: Arc<insights::Insights>,
    /// Sponsors.
    pub sponsors: Arc<sponsors::Sponsors>,
    /// Activity derived from the event logs (graphs, feed, achievements).
    pub activity: Arc<activity::Activity>,
    /// Signing keys of user accounts.
    pub keys: Arc<keys::SigningKeys>,
    /// The ports in use.
    pub ports: Ports,
    /// Settings.
    pub config: CommunityConfig,
}

impl Community {
    /// Builds every store over `db` and the ports. `resolver` turns revisions into changesets
    /// (the forge's own hook); `access` filters who the fan-out may notify.
    pub fn new(
        db: Db,
        ports: Ports,
        directory: Arc<dyn PrincipalDirectory>,
        resolver: Arc<dyn RevisionResolver>,
        access: Arc<dyn notifications::ReadAccess>,
        config: CommunityConfig,
    ) -> Community {
        let base = config.base_url.clone();
        let users = Arc::new(users::Users::new(
            db.clone(),
            directory.clone(),
            ports.signer.clone(),
            config.users.clone(),
        ));
        let discussions = Arc::new(discussions::Discussions::new(db.clone(), &base));
        let releases = Arc::new(releases::Releases::new(
            db.clone(),
            ports.tags.clone(),
            ports.changes.clone(),
            ports.vcs.clone(),
            resolver,
            ports.signer.clone(),
            discussions.clone(),
            &base,
        ));
        let notifications = Arc::new(notifications::Notifications::new(
            db.clone(),
            directory,
            access,
            users.clone(),
            config.notify.clone(),
        ));
        Community {
            reactions: Arc::new(reactions::Reactions::new(db.clone())),
            wiki: Arc::new(wiki::Wiki::new(db.clone(), ports.vcs.clone(), &base)),
            gists: Arc::new(gists::Gists::new(
                db.clone(),
                ports.vcs.clone(),
                ports.signer.clone(),
                &base,
            )),
            insights: Arc::new(insights::Insights::new(
                db.clone(),
                ports.activity.clone(),
                ports.deps.clone(),
                users.clone(),
                releases.clone(),
            )),
            activity: Arc::new(activity::Activity::new(db.clone())),
            keys: Arc::new(keys::SigningKeys::new(db.clone())),
            sponsors: Arc::new(sponsors::Sponsors::new(
                db,
                ports.payments.clone(),
                ports.vcs.clone(),
                &base,
            )),
            discussions,
            releases,
            notifications,
            users,
            ports,
            config,
        }
    }

    /// One pass of the background work: derive notifications from new events, fire due
    /// reminders, index new activity. Deterministic progress for tests and `--dev` tooling.
    pub async fn flush(&self, cx: &Cx, events: &EventLog) -> Result<()> {
        self.notifications.flush(cx, events).await?;
        self.activity.flush(cx, events).await?;
        Ok(())
    }

    /// One maintenance pass: inbox and queue pruning, account purges, sponsorship expiry and
    /// traffic retention.
    pub async fn maintain(&self, cx: &Cx) -> Result<()> {
        self.notifications.maintain(cx).await?;
        self.users.purge_expired(cx).await?;
        self.sponsors.maintain(cx).await?;
        self.insights.prune_traffic(cx).await?;
        Ok(())
    }
}
