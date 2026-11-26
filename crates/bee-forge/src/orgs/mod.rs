//! Orgs, members, roles, invitations, org settings and custom property definitions
//! (SPEC §11.1, PRODUCT §4.20); repos and everything hanging off them live in [`repos`].
//!
//! # Names
//!
//! Org and repo names are 1–100 characters of `A-Z a-z 0-9 . _ -`, not starting with `.`,
//! unique case-insensitively. Renames change only the name; ids are untouched, and the old
//! name redirects for [`REDIRECT_DAYS`] days (SPEC §11.1).
//!
//! # Delete and restore
//!
//! Deleting an org or repo is a soft delete: it disappears from reads and lists at once and
//! can be restored for [`RESTORE_DAYS`] days. A deleted org keeps its name reserved; a deleted
//! repo releases its name, and restoring it fails with `ALREADY_EXISTS` if the name was
//! taken meanwhile. [`Orgs::purge_expired`] removes rows past the window.
//!
//! # Access
//!
//! [`Orgs::effective_role`] computes a principal's repo role from every source: org owners
//! and admins are repo admins, other members get the org's base role, direct collaborators
//! their grant, everyone may read public repos and members may read internal ones, plus any
//! registered [`AccessSource`] (teams, apps). The highest role wins; the sources are
//! reported (`org:owner`, `org:base`, `direct`, `public`, ...).

pub mod repos;

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_proto::v1::{
    CustomPropertyDefinition, CustomPropertyType, Event, InvitationState, Member, Org,
    OrgInvitation, OrgSettings, OutsideCollaborator, Principal, Repo, RepoRole, Role, SubjectKind,
};

use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::module::{EventKind, ForgeModule};
use crate::store::{
    Cursor, Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, Surface, TableDef, Tx,
};

/// Days an old name redirects.
pub const REDIRECT_DAYS: u64 = 90;
/// Days a deleted org or repo can be restored.
pub const RESTORE_DAYS: u64 = 90;
/// Days an invitation stays pending.
pub const INVITATION_DAYS: u64 = 7;
const DAY_MS: u64 = 86_400_000;

macro_rules! table {
    ($name:ident, $table:literal, $uk1:expr, $uk2:expr, [$($idx:ident),*], $doc:literal) => {
        #[doc = $doc]
        pub static $name: TableDef = TableDef {
            name: $table,
            surface: Surface::ORGS,
            unique_k1: $uk1,
            unique_k2: $uk2,
            indexes: &[$(Index::$idx),*],
        };
    };
}

table!(
    ORGS,
    "forge_orgs",
    true,
    false,
    [],
    "Orgs (global scope); `k1` lower-case name, `n1` deletion time."
);
table!(
    ORG_REDIRECTS,
    "forge_org_redirects",
    false,
    false,
    [],
    "Old org names; `k1` org id, `n1` expiry."
);
table!(
    ORG_SETTINGS,
    "forge_org_settings",
    false,
    false,
    [],
    "Org settings by org id."
);
table!(
    MEMBERS,
    "forge_org_members",
    false,
    false,
    [K2],
    "Members, scoped by org; `k1` role, `k2` principal id."
);
table!(
    ORG_INVITES,
    "forge_org_invitations",
    false,
    false,
    [K1, K2],
    "Org invitations; `k1` org id, `k2` invitee key."
);
table!(
    PROP_DEFS,
    "forge_custom_property_defs",
    false,
    false,
    [],
    "Custom property definitions, scoped by org."
);
table!(
    REPOS,
    "forge_repos",
    false,
    true,
    [K1],
    "Repos (global scope); `k1` org id, `k2` `org_id/lower(name)` while live, `n1` deletion time."
);
table!(
    REPO_REDIRECTS,
    "forge_repo_redirects",
    false,
    false,
    [K1],
    "Old repo names `org_id/lower(name)`; `k1` repo id, `n1` expiry."
);
table!(
    REPO_SETTINGS,
    "forge_repo_settings",
    false,
    false,
    [],
    "Repo settings by repo id."
);
table!(
    COLLABORATORS,
    "forge_repo_collaborators",
    false,
    false,
    [K2],
    "Direct repo grants, scoped by repo; `k2` principal id."
);
table!(
    REPO_INVITES,
    "forge_repo_invitations",
    false,
    false,
    [K1, K2],
    "Repo invitations; `k1` repo id, `k2` invitee principal id."
);
table!(
    AUTOLINKS,
    "forge_repo_autolinks",
    true,
    false,
    [],
    "Autolinks, scoped by repo; `k1` key prefix."
);
table!(
    RESOLVERS,
    "forge_repo_resolver_hooks",
    false,
    false,
    [],
    "Resolver hooks by repo id."
);
table!(
    MIRRORS,
    "forge_repo_mirrors",
    false,
    false,
    [],
    "Mirrors, scoped by repo."
);
table!(
    DEPLOY_KEYS,
    "forge_repo_deploy_keys",
    true,
    false,
    [],
    "Deploy keys, scoped by repo; `k1` key fingerprint."
);

static M1: Migration = Migration {
    module: "orgs",
    version: 1,
    name: "create_orgs_and_repos",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&ORGS),
        MigrationOp::CreateTable(&ORG_REDIRECTS),
        MigrationOp::CreateTable(&ORG_SETTINGS),
        MigrationOp::CreateTable(&MEMBERS),
        MigrationOp::CreateTable(&ORG_INVITES),
        MigrationOp::CreateTable(&PROP_DEFS),
        MigrationOp::CreateTable(&REPOS),
        MigrationOp::CreateTable(&REPO_REDIRECTS),
        MigrationOp::CreateTable(&REPO_SETTINGS),
        MigrationOp::CreateTable(&COLLABORATORS),
        MigrationOp::CreateTable(&REPO_INVITES),
        MigrationOp::CreateTable(&AUTOLINKS),
        MigrationOp::CreateTable(&RESOLVERS),
        MigrationOp::CreateTable(&MIRRORS),
        MigrationOp::CreateTable(&DEPLOY_KEYS),
    ],
};

/// What the orgs module owns.
pub static MODULE: ForgeModule = ForgeModule {
    name: "orgs",
    surfaces: &[Surface::ORGS],
    migrations: &[&M1],
    event_kinds: &[
        EventKind::new("org.created", "An org was created"),
        EventKind::gh(
            "org.updated",
            "Org profile changed",
            "organization",
            "updated",
        ),
        EventKind::gh(
            "org.renamed",
            "An org was renamed",
            "organization",
            "renamed",
        ),
        EventKind::gh(
            "org.deleted",
            "An org was soft-deleted",
            "organization",
            "deleted",
        ),
        EventKind::new("org.restored", "A deleted org was restored"),
        EventKind::new("org.settings_updated", "Org settings changed"),
        EventKind::gh(
            "member.added",
            "A member joined",
            "organization",
            "member_added",
        ),
        EventKind::gh(
            "member.removed",
            "A member left",
            "organization",
            "member_removed",
        ),
        EventKind::new("member.role_changed", "A member's role changed"),
        EventKind::gh(
            "org_invitation.created",
            "Someone was invited",
            "organization",
            "member_invited",
        ),
        EventKind::new("org_invitation.accepted", "An invitation was accepted"),
        EventKind::new("org_invitation.declined", "An invitation was declined"),
        EventKind::new("org_invitation.cancelled", "An invitation was cancelled"),
        EventKind::new(
            "custom_property.updated",
            "A custom property definition changed",
        ),
        EventKind::gh(
            "repo.created",
            "A repo was created",
            "repository",
            "created",
        ),
        EventKind::gh(
            "repo.updated",
            "Repo metadata changed",
            "repository",
            "edited",
        ),
        EventKind::gh(
            "repo.renamed",
            "A repo was renamed",
            "repository",
            "renamed",
        ),
        EventKind::gh(
            "repo.archived",
            "A repo was archived",
            "repository",
            "archived",
        ),
        EventKind::gh(
            "repo.unarchived",
            "A repo was unarchived",
            "repository",
            "unarchived",
        ),
        EventKind::gh(
            "repo.deleted",
            "A repo was soft-deleted",
            "repository",
            "deleted",
        ),
        EventKind::new("repo.restored", "A deleted repo was restored"),
        EventKind::gh(
            "repo.transferred",
            "A repo moved orgs",
            "repository",
            "transferred",
        ),
        EventKind::new("repo.settings_updated", "Repo settings changed"),
        EventKind::new("repo.topics_updated", "Repo topics changed"),
        EventKind::new(
            "repo.properties_updated",
            "Repo custom property values changed",
        ),
        EventKind::gh(
            "collaborator.added",
            "A collaborator was added",
            "member",
            "added",
        ),
        EventKind::gh(
            "collaborator.removed",
            "A collaborator was removed",
            "member",
            "removed",
        ),
        EventKind::new("repo_invitation.created", "A repo invitation was sent"),
        EventKind::new("repo_invitation.accepted", "A repo invitation was accepted"),
        EventKind::new("repo_invitation.declined", "A repo invitation was declined"),
        EventKind::new("autolink.created", "An autolink was added"),
        EventKind::new("autolink.deleted", "An autolink was removed"),
        EventKind::new("resolver_hook.updated", "The resolver hook changed"),
        EventKind::new("mirror.created", "A mirror was added"),
        EventKind::new("mirror.deleted", "A mirror was removed"),
        EventKind::gh(
            "deploy_key.created",
            "A deploy key was added",
            "deploy_key",
            "created",
        ),
        EventKind::gh(
            "deploy_key.deleted",
            "A deploy key was removed",
            "deploy_key",
            "deleted",
        ),
    ],
    audit_prefixes: &[
        "org.",
        "member.",
        "org_invitation.",
        "custom_property.",
        "repo.",
        "collaborator.",
        "repo_invitation.",
        "autolink.",
        "resolver_hook.",
        "mirror.",
        "deploy_key.",
    ],
};

/// Validates an org or repo name.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(ForgeError::invalid(
            "invalid_name",
            format!(
                "{name:?} is not a valid name (1-100 of A-Z a-z 0-9 . _ -, not starting with .)"
            ),
        ))
    }
}

/// Rank of a repo role (higher grants more).
pub fn role_rank(r: RepoRole) -> u8 {
    match r {
        RepoRole::Unspecified | RepoRole::None => 0,
        RepoRole::Read => 1,
        RepoRole::Triage => 2,
        RepoRole::Write => 3,
        RepoRole::Maintain => 4,
        RepoRole::Admin => 5,
    }
}

/// The policy role name of a repo role (`read` .. `admin`), if any.
pub fn role_name(r: RepoRole) -> Option<&'static str> {
    match r {
        RepoRole::Read => Some("read"),
        RepoRole::Triage => Some("triage"),
        RepoRole::Write => Some("write"),
        RepoRole::Maintain => Some("maintain"),
        RepoRole::Admin => Some("admin"),
        RepoRole::Unspecified | RepoRole::None => None,
    }
}

fn org_role_name(r: Role) -> &'static str {
    match r {
        Role::Owner => "owner",
        Role::Admin => "admin",
        Role::Member => "member",
        Role::BillingManager => "billing_manager",
        Role::SecurityManager => "security_manager",
        Role::Moderator => "moderator",
        Role::AppManager => "app_manager",
        Role::Unspecified => "",
    }
}

/// Extra sources of repo access (teams, apps) registered by later modules.
#[async_trait]
pub trait AccessSource: Send + Sync {
    /// Roles `principal_id` holds on `repo` through this source, with a source label each
    /// (e.g. `team:core`).
    async fn grants(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<Vec<(RepoRole, String)>>;
}

/// Default org settings.
pub fn default_settings(org_id: &str) -> OrgSettings {
    OrgSettings {
        org_id: org_id.into(),
        base_repo_role: RepoRole::Read as i32,
        members_can_create_repos: true,
        members_can_create_public_repos: true,
        members_can_fork_private_repos: false,
        members_can_delete_repos: false,
        members_can_invite_outside_collaborators: false,
        members_can_create_teams: true,
        web_commit_signoff_required: false,
        readme_repo_id: String::new(),
        pinned_repo_ids: Vec::new(),
        max_open_changes_per_agent: 0,
        version: 1,
    }
}

/// A partial org profile update.
#[derive(Clone, Debug, Default)]
pub struct OrgPatch {
    /// Display name.
    pub display: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Website.
    pub website: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Location.
    pub location: Option<String>,
}

/// Who an org invitation is for.
#[derive(Clone, Debug, Default)]
pub struct Invitee {
    /// A known principal.
    pub principal: Option<Principal>,
    /// A login (resolved on acceptance).
    pub login: String,
    /// An email address.
    pub email: String,
}

impl Invitee {
    fn key(&self) -> Result<String> {
        if let Some(p) = &self.principal {
            Ok(p.id.clone())
        } else if !self.login.is_empty() {
            Ok(format!("login:{}", self.login.to_lowercase()))
        } else if !self.email.is_empty() {
            Ok(format!("email:{}", self.email.to_lowercase()))
        } else {
            Err(ForgeError::missing("invitee_principal_id"))
        }
    }
}

/// The caller of an invitation response.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    /// Principal.
    pub principal: Principal,
    /// Login names it is known by.
    pub logins: Vec<String>,
    /// Verified emails.
    pub emails: Vec<String>,
}

impl Caller {
    fn matches(&self, key: &str) -> bool {
        key == self.principal.id
            || key
                .strip_prefix("login:")
                .is_some_and(|l| self.logins.iter().any(|x| x.eq_ignore_ascii_case(l)))
            || key
                .strip_prefix("email:")
                .is_some_and(|e| self.emails.iter().any(|x| x.eq_ignore_ascii_case(e)))
    }
}

fn page_cursor(scope: &str, after: &str) -> Option<Cursor> {
    (!after.is_empty()).then(|| Cursor {
        n1: 0,
        scope: scope.to_string(),
        id: after.to_string(),
    })
}

fn next_token<T>(items: &[T], more: bool, id: impl Fn(&T) -> String) -> String {
    if more {
        items.last().map(id).unwrap_or_default()
    } else {
        String::new()
    }
}

fn org_json(o: &Org) -> String {
    serde_json::json!({
        "id": o.id, "name": o.name, "display": o.display, "description": o.description,
        "website": o.website, "email": o.email, "location": o.location,
    })
    .to_string()
}

/// Orgs and repos over the store.
pub struct Orgs {
    db: Db,
    sources: RwLock<Vec<Arc<dyn AccessSource>>>,
    mirror_syncer: RwLock<Option<Arc<dyn MirrorSyncer>>>,
    insecure_mirrors: std::sync::atomic::AtomicBool,
}

/// Runs one sync of a repo mirror (the server's git worker: fetch and import for pull
/// mirrors, push for push mirrors) and reports the mirror with its new status.
#[async_trait::async_trait]
pub trait MirrorSyncer: Send + Sync {
    /// Syncs mirror `mirror_id` of `repo` now.
    async fn sync(
        &self,
        repo: &bee_proto::v1::Repo,
        mirror_id: &str,
    ) -> Result<bee_proto::v1::RepoMirror>;
}

impl Orgs {
    /// Orgs over `db`.
    pub fn new(db: Db) -> Orgs {
        Orgs {
            db,
            sources: RwLock::new(Vec::new()),
            mirror_syncer: RwLock::new(None),
            insecure_mirrors: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Installs the mirror syncer `RepoService.SyncRepoMirror` runs.
    pub fn set_mirror_syncer(&self, s: Arc<dyn MirrorSyncer>) {
        if let Ok(mut v) = self.mirror_syncer.write() {
            *v = Some(s);
        }
    }

    /// The installed mirror syncer.
    pub fn mirror_syncer(&self) -> Option<Arc<dyn MirrorSyncer>> {
        self.mirror_syncer.read().ok().and_then(|v| v.clone())
    }

    /// Whether mirrors may use `http://` and `file://` remotes (development and tests).
    pub fn set_insecure_mirrors(&self, allow: bool) {
        self.insecure_mirrors
            .store(allow, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn insecure_mirrors(&self) -> bool {
        self.insecure_mirrors
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Registers an extra access source (teams, apps).
    pub fn add_access_source(&self, s: Arc<dyn AccessSource>) {
        if let Ok(mut v) = self.sources.write() {
            v.push(s);
        }
    }

    pub(crate) fn db(&self) -> &Db {
        &self.db
    }

    async fn org_event(tx: &mut Tx, kind: &str, org: &Org) -> Result<()> {
        tx.emit_event(Event {
            kind: kind.into(),
            org_id: org.id.clone(),
            payload_json: serde_json::to_string(org).unwrap_or_default(),
            subject: Some(bee_proto::v1::SubjectRef {
                kind: SubjectKind::Org as i32,
                repo: None,
                id: org.id.clone(),
            }),
            ..Default::default()
        })
        .await
    }

    async fn simple_event(tx: &mut Tx, kind: &str, org_id: &str, payload: String) -> Result<()> {
        tx.emit_event(Event {
            kind: kind.into(),
            org_id: org_id.into(),
            payload_json: payload,
            ..Default::default()
        })
        .await
    }

    /// Creates an org; the caller becomes its owner.
    pub async fn create_org(
        &self,
        cx: &Cx,
        name: &str,
        display: &str,
        creator: Principal,
    ) -> Result<Org> {
        self.create_org_with_id(cx, "", name, display, creator)
            .await
    }

    /// Creates an org under a preallocated id (empty = a new ULID); the caller becomes its
    /// owner. The server uses it to give an org that already exists on the VCS side (created
    /// by dev bootstrap or an import) its forge row under the same id.
    pub async fn create_org_with_id(
        &self,
        cx: &Cx,
        id: &str,
        name: &str,
        display: &str,
        creator: Principal,
    ) -> Result<Org> {
        validate_name(name)?;
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let lower = name.to_lowercase();
        let taken = tx
            .scan(&ORGS, &Scan::scope("", 1).key(Key::K1(lower.clone())))
            .await?;
        if !taken.is_empty() {
            return Err(ForgeError::already_exists(format!("org {name}")));
        }
        if !id.is_empty() && tx.get(&ORGS, "", id).await?.is_some() {
            return Err(ForgeError::already_exists(format!("org id {id}")));
        }
        let now = tx.now_ms();
        let id = if id.is_empty() {
            tx.new_id()?
        } else {
            id.to_string()
        };
        let org = Org {
            id: id.clone(),
            name: name.into(),
            display: if display.is_empty() {
                name.into()
            } else {
                display.into()
            },
            created_at: Some(ts(now)),
            version: 1,
            ..Default::default()
        };
        tx.insert(&ORGS, &Row::new("", id.clone()).k1(lower.clone()).msg(&org))
            .await?;
        tx.delete(&ORG_REDIRECTS, "", &lower).await?;
        tx.insert(
            &ORG_SETTINGS,
            &Row::new("", id.clone()).msg(&default_settings(&id)),
        )
        .await?;
        let member = Member {
            principal: Some(creator.clone()),
            role: Role::Owner as i32,
            joined_at: Some(ts(now)),
            ..Default::default()
        };
        tx.insert(
            &MEMBERS,
            &Row::new(id.clone(), creator.id.clone())
                .k1("owner")
                .k2(creator.id.clone())
                .msg(&member),
        )
        .await?;
        Self::org_event(&mut tx, "org.created", &org).await?;
        tx.audit(
            AuditDraft::new("org.create", &id)
                .target(SubjectKind::Org, &id)
                .after(&org_json(&org)),
        )
        .await?;
        tx.commit().await?;
        Ok(org)
    }

    async fn load_org(tx: &mut Tx, id: &str) -> Result<Option<(Org, Row)>> {
        Ok(tx.get_msg::<Org>(&ORGS, "", id).await?.map(|(mut o, r)| {
            o.version = r.version;
            (o, r)
        }))
    }

    /// An org by id, including soft-deleted ones.
    pub async fn org_any(&self, cx: &Cx, id: &str) -> Result<Org> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        Self::load_org(&mut tx, id)
            .await?
            .map(|(o, _)| o)
            .ok_or_else(|| ForgeError::not_found(format!("org {id}")))
    }

    pub(crate) async fn find_org(tx: &mut Tx, name_or_id: &str, now: u64) -> Result<Option<Org>> {
        if let Some((o, _)) = Self::load_org(tx, name_or_id).await? {
            return Ok(Some(o));
        }
        let lower = name_or_id.to_lowercase();
        let by_name = tx
            .scan(&ORGS, &Scan::scope("", 1).key(Key::K1(lower.clone())))
            .await?;
        if let Some(r) = by_name.first() {
            let mut o: Org = r.decode()?;
            o.version = r.version;
            return Ok(Some(o));
        }
        if let Some(redirect) = tx.get(&ORG_REDIRECTS, "", &lower).await?
            && redirect.n1.unwrap_or(0) as u64 > now
            && let Some(target) = redirect.k1.as_deref()
        {
            return Ok(Self::load_org(tx, target).await?.map(|(o, _)| o));
        }
        Ok(None)
    }

    /// A live org by name (following rename redirects) or id.
    pub async fn get_org(&self, cx: &Cx, name_or_id: &str) -> Result<Org> {
        if name_or_id.is_empty() {
            return Err(ForgeError::missing("org"));
        }
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let now = tx.now_ms();
        match Self::find_org(&mut tx, name_or_id, now).await? {
            Some(o) if o.deleted_at.is_none() => Ok(o),
            _ => Err(ForgeError::not_found(format!("org {name_or_id}"))),
        }
    }

    /// Orgs `principal_id` is a member of, by org id.
    pub async fn orgs_of(
        &self,
        cx: &Cx,
        principal_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Org>, String)> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let rows = tx
            .scan(
                &MEMBERS,
                &Scan {
                    key: Some(Key::K2(principal_id.into())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        let mut ids: Vec<String> = rows
            .into_iter()
            .map(|r| r.scope)
            .filter(|s| s.as_str() > after)
            .collect();
        ids.sort();
        let more = ids.len() > limit;
        let mut out = Vec::new();
        for id in ids.into_iter().take(limit) {
            if let Some((o, _)) = Self::load_org(&mut tx, &id).await?
                && o.deleted_at.is_none()
            {
                out.push(o);
            }
        }
        let next = next_token(&out, more, |o| o.id.clone());
        Ok((out, next))
    }

    /// Updates the org profile (CAS on `expected_version` unless 0).
    pub async fn update_org(
        &self,
        cx: &Cx,
        id: &str,
        p: OrgPatch,
        expected_version: u64,
    ) -> Result<Org> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let Some((mut org, row)) = Self::load_org(&mut tx, id).await? else {
            return Err(ForgeError::not_found(format!("org {id}")));
        };
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        let before = org_json(&org);
        if let Some(v) = p.display {
            org.display = v;
        }
        if let Some(v) = p.description {
            org.description = v;
        }
        if let Some(v) = p.website {
            org.website = v;
        }
        if let Some(v) = p.email {
            org.email = v;
        }
        if let Some(v) = p.location {
            org.location = v;
        }
        org.version = tx.update(&ORGS, &row.clone().msg(&org)).await?;
        Self::org_event(&mut tx, "org.updated", &org).await?;
        tx.audit(
            AuditDraft::new("org.update", id)
                .target(SubjectKind::Org, id)
                .before(&before)
                .after(&org_json(&org)),
        )
        .await?;
        tx.commit().await?;
        Ok(org)
    }

    /// Renames an org; the old name redirects for [`REDIRECT_DAYS`].
    pub async fn rename_org(&self, cx: &Cx, id: &str, new_name: &str) -> Result<Org> {
        validate_name(new_name)?;
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let Some((mut org, row)) = Self::load_org(&mut tx, id).await? else {
            return Err(ForgeError::not_found(format!("org {id}")));
        };
        let lower = new_name.to_lowercase();
        let old_lower = org.name.to_lowercase();
        if lower != old_lower {
            let taken = tx
                .scan(&ORGS, &Scan::scope("", 1).key(Key::K1(lower.clone())))
                .await?;
            if !taken.is_empty() {
                return Err(ForgeError::already_exists(format!("org {new_name}")));
            }
        }
        let before = org_json(&org);
        let now = tx.now_ms();
        org.name = new_name.into();
        org.version = tx
            .update(&ORGS, &row.clone().k1(lower.clone()).msg(&org))
            .await?;
        if lower != old_lower {
            tx.delete(&ORG_REDIRECTS, "", &lower).await?;
            tx.put(
                &ORG_REDIRECTS,
                &Row::new("", old_lower)
                    .k1(id)
                    .n1((now + REDIRECT_DAYS * DAY_MS) as i64),
            )
            .await?;
        }
        Self::org_event(&mut tx, "org.renamed", &org).await?;
        tx.audit(
            AuditDraft::new("org.rename", id)
                .target(SubjectKind::Org, id)
                .before(&before)
                .after(&org_json(&org)),
        )
        .await?;
        tx.commit().await?;
        Ok(org)
    }

    /// Soft-deletes an org; returns the end of the restore window (ms).
    pub async fn delete_org(&self, cx: &Cx, id: &str, confirm_name: &str) -> Result<u64> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let Some((mut org, row)) = Self::load_org(&mut tx, id).await? else {
            return Err(ForgeError::not_found(format!("org {id}")));
        };
        if org.deleted_at.is_some() {
            return Err(ForgeError::not_found(format!("org {id}")));
        }
        if confirm_name != org.name {
            return Err(ForgeError::invalid(
                "confirmation_mismatch",
                "confirm_name must equal the org name",
            ));
        }
        let now = tx.now_ms();
        org.deleted_at = Some(ts(now));
        tx.update(&ORGS, &row.clone().n1(now as i64).msg(&org))
            .await?;
        Self::org_event(&mut tx, "org.deleted", &org).await?;
        tx.audit(AuditDraft::new("org.delete", id).target(SubjectKind::Org, id))
            .await?;
        tx.commit().await?;
        Ok(now + RESTORE_DAYS * DAY_MS)
    }

    /// Restores a soft-deleted org within the window.
    pub async fn restore_org(&self, cx: &Cx, id: &str) -> Result<Org> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let Some((mut org, row)) = Self::load_org(&mut tx, id).await? else {
            return Err(ForgeError::not_found(format!("org {id}")));
        };
        let Some(deleted) = row.n1 else {
            return Err(ForgeError::invalid_state("org is not deleted"));
        };
        if tx.now_ms() > deleted as u64 + RESTORE_DAYS * DAY_MS {
            return Err(ForgeError::not_found(format!(
                "org {id} (restore window passed)"
            )));
        }
        org.deleted_at = None;
        let mut r = row.clone().msg(&org);
        r.n1 = None;
        org.version = tx.update(&ORGS, &r).await?;
        Self::org_event(&mut tx, "org.restored", &org).await?;
        tx.audit(AuditDraft::new("org.restore", id).target(SubjectKind::Org, id))
            .await?;
        tx.commit().await?;
        Ok(org)
    }

    /// Org settings.
    pub async fn settings(&self, cx: &Cx, org_id: &str) -> Result<OrgSettings> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        Ok(
            match tx.get_msg::<OrgSettings>(&ORG_SETTINGS, "", org_id).await? {
                Some((mut s, r)) => {
                    s.version = r.version;
                    s
                }
                None => default_settings(org_id),
            },
        )
    }

    /// Replaces org settings after `f` edits them (CAS on `expected_version` unless 0).
    pub async fn update_settings(
        &self,
        cx: &Cx,
        org_id: &str,
        expected_version: u64,
        f: impl FnOnce(&mut OrgSettings) + Send,
    ) -> Result<OrgSettings> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let (mut s, row) = match tx.get_msg::<OrgSettings>(&ORG_SETTINGS, "", org_id).await? {
            Some(x) => x,
            None => {
                let s = default_settings(org_id);
                let row = Row::new("", org_id).msg(&s);
                tx.insert(&ORG_SETTINGS, &row).await?;
                (s, row.version(1))
            }
        };
        if expected_version != 0 && expected_version != row.version {
            return Err(ForgeError::version_mismatch(expected_version, row.version));
        }
        let before = serde_json::to_string(&s).unwrap_or_default();
        f(&mut s);
        s.org_id = org_id.into();
        s.version = tx.update(&ORG_SETTINGS, &row.clone().msg(&s)).await?;
        let after = serde_json::to_string(&s).unwrap_or_default();
        Self::simple_event(&mut tx, "org.settings_updated", org_id, after.clone()).await?;
        tx.audit(
            AuditDraft::new("org.settings_update", org_id)
                .target(SubjectKind::Org, org_id)
                .before(&before)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// A member, if `principal_id` is one.
    pub async fn member(
        &self,
        cx: &Cx,
        org_id: &str,
        principal_id: &str,
    ) -> Result<Option<Member>> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        Ok(tx
            .get_msg::<Member>(&MEMBERS, org_id, principal_id)
            .await?
            .map(|(m, _)| m))
    }

    /// Members by principal id, paged.
    pub async fn members(
        &self,
        cx: &Cx,
        org_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Member>, String)> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let rows = tx
            .scan_msgs::<Member>(
                &MEMBERS,
                &Scan::scope(org_id, limit + 1).after(page_cursor(org_id, after)),
            )
            .await?;
        let more = rows.len() > limit;
        let out: Vec<Member> = rows.into_iter().take(limit).map(|(m, _)| m).collect();
        let next = next_token(&out, more, |m| {
            m.principal
                .as_ref()
                .map(|p| p.id.clone())
                .unwrap_or_default()
        });
        Ok((out, next))
    }

    async fn owner_count(tx: &mut Tx, org_id: &str) -> Result<u64> {
        tx.count(&MEMBERS, org_id, Some(&Key::K1("owner".into())))
            .await
    }

    /// Adds, re-roles or (with `Role::Unspecified`) removes a member. The last owner can be
    /// neither removed nor demoted (`INVALID_STATE`).
    pub async fn set_member(
        &self,
        cx: &Cx,
        org_id: &str,
        principal: Principal,
        role: Role,
    ) -> Result<Option<Member>> {
        if principal.id.is_empty() {
            return Err(ForgeError::missing("principal_id"));
        }
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let mut existing = tx
            .get_msg::<Member>(&MEMBERS, org_id, &principal.id)
            .await?;
        let may_reduce_owners = existing
            .as_ref()
            .is_some_and(|(m, _)| m.role == Role::Owner as i32);
        if may_reduce_owners && role != Role::Owner {
            // Serialize owner demotions and removals on the org row before counting owners.
            // The org and membership tables share the ORGS surface, so this lock is held
            // through the member write without introducing a guard row into member queries.
            tx.get_for_update(&ORGS, "", org_id).await?;
            existing = tx
                .get_msg::<Member>(&MEMBERS, org_id, &principal.id)
                .await?;
            let was_owner = existing
                .as_ref()
                .is_some_and(|(m, _)| m.role == Role::Owner as i32);
            if was_owner && Self::owner_count(&mut tx, org_id).await? <= 1 {
                return Err(ForgeError::invalid_state("an org needs at least one owner"));
            }
        }
        let now = tx.now_ms();
        let result = if role == Role::Unspecified {
            if existing.is_none() {
                return Err(ForgeError::not_found(format!("member {}", principal.id)));
            }
            tx.delete(&MEMBERS, org_id, &principal.id).await?;
            Self::simple_event(
                &mut tx,
                "member.removed",
                org_id,
                serde_json::json!({"principal_id": principal.id}).to_string(),
            )
            .await?;
            tx.audit(
                AuditDraft::new("member.remove", org_id).target(SubjectKind::User, &principal.id),
            )
            .await?;
            None
        } else {
            let (member, kind, action) = match existing {
                Some((mut m, row)) => {
                    m.role = role as i32;
                    tx.update(&MEMBERS, &row.clone().k1(org_role_name(role)).msg(&m))
                        .await?;
                    (m, "member.role_changed", "member.role_change")
                }
                None => {
                    let m = Member {
                        principal: Some(principal.clone()),
                        role: role as i32,
                        joined_at: Some(ts(now)),
                        ..Default::default()
                    };
                    tx.insert(
                        &MEMBERS,
                        &Row::new(org_id, principal.id.clone())
                            .k1(org_role_name(role))
                            .k2(principal.id.clone())
                            .msg(&m),
                    )
                    .await?;
                    (m, "member.added", "member.add")
                }
            };
            let after =
                serde_json::json!({"principal_id": principal.id, "role": org_role_name(role)})
                    .to_string();
            Self::simple_event(&mut tx, kind, org_id, after.clone()).await?;
            tx.audit(
                AuditDraft::new(action, org_id)
                    .target(SubjectKind::User, &principal.id)
                    .after(&after),
            )
            .await?;
            Some(member)
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Sets a member's built-in role or adds a custom role id.
    pub async fn assign_role(
        &self,
        cx: &Cx,
        org_id: &str,
        principal_id: &str,
        role: Role,
        custom_role_id: &str,
    ) -> Result<Member> {
        let current = self
            .member(cx, org_id, principal_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("member {principal_id}")))?;
        if custom_role_id.is_empty() {
            if role == Role::Unspecified {
                return Err(ForgeError::missing("role"));
            }
            let p = current.principal.clone().unwrap_or_default();
            return self
                .set_member(cx, org_id, p, role)
                .await?
                .ok_or_else(|| ForgeError::internal("member vanished"));
        }
        self.edit_custom_roles(cx, org_id, principal_id, custom_role_id, true)
            .await
    }

    /// Removes a custom role id from a member.
    pub async fn revoke_role(
        &self,
        cx: &Cx,
        org_id: &str,
        principal_id: &str,
        custom_role_id: &str,
    ) -> Result<Member> {
        if custom_role_id.is_empty() {
            return Err(ForgeError::missing("custom_role_id"));
        }
        self.edit_custom_roles(cx, org_id, principal_id, custom_role_id, false)
            .await
    }

    async fn edit_custom_roles(
        &self,
        cx: &Cx,
        org_id: &str,
        principal_id: &str,
        role_id: &str,
        add: bool,
    ) -> Result<Member> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let Some((mut m, row)) = tx.get_msg::<Member>(&MEMBERS, org_id, principal_id).await? else {
            return Err(ForgeError::not_found(format!("member {principal_id}")));
        };
        let had = m.custom_role_ids.iter().any(|r| r == role_id);
        if add && !had {
            m.custom_role_ids.push(role_id.into());
            m.custom_role_ids.sort();
        } else if !add {
            if !had {
                return Err(ForgeError::not_found(format!(
                    "custom role {role_id} on {principal_id}"
                )));
            }
            m.custom_role_ids.retain(|r| r != role_id);
        }
        tx.update(&MEMBERS, &row.clone().msg(&m)).await?;
        let after =
            serde_json::json!({"principal_id": principal_id, "custom_role_ids": m.custom_role_ids})
                .to_string();
        Self::simple_event(&mut tx, "member.role_changed", org_id, after.clone()).await?;
        tx.audit(
            AuditDraft::new(
                if add {
                    "member.custom_role_assign"
                } else {
                    "member.custom_role_revoke"
                },
                org_id,
            )
            .target(SubjectKind::User, principal_id)
            .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(m)
    }

    /// Invites someone to the org.
    pub async fn invite(
        &self,
        cx: &Cx,
        org_id: &str,
        invitee: Invitee,
        role: Role,
        team_ids: Vec<String>,
    ) -> Result<OrgInvitation> {
        let key = invitee.key()?;
        let role = if role == Role::Unspecified {
            Role::Member
        } else {
            role
        };
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        if let Some(p) = &invitee.principal
            && tx.get(&MEMBERS, org_id, &p.id).await?.is_some()
        {
            return Err(ForgeError::already_exists(format!("member {}", p.id)));
        }
        let now = tx.now_ms();
        let pending = tx
            .scan_msgs::<OrgInvitation>(
                &ORG_INVITES,
                &Scan::scope("", 100).key(Key::K2(key.clone())),
            )
            .await?;
        if pending.iter().any(|(i, r)| {
            i.org_id == org_id
                && i.state == InvitationState::Pending as i32
                && r.n1.unwrap_or(0) as u64 > now
        }) {
            return Err(ForgeError::already_exists(
                "a pending invitation for this invitee",
            ));
        }
        let inv = OrgInvitation {
            id: tx.new_id()?,
            org_id: org_id.into(),
            invitee_login: invitee.login.clone(),
            invitee_email: invitee.email.clone(),
            invitee_principal_id: invitee
                .principal
                .as_ref()
                .map(|p| p.id.clone())
                .unwrap_or_default(),
            role: role as i32,
            team_ids,
            inviter: Some(cx.principal()),
            state: InvitationState::Pending as i32,
            created_at: Some(ts(now)),
            expires_at: Some(ts(now + INVITATION_DAYS * DAY_MS)),
        };
        tx.insert(
            &ORG_INVITES,
            &Row::new("", inv.id.clone())
                .k1(org_id)
                .k2(key)
                .n1((now + INVITATION_DAYS * DAY_MS) as i64)
                .msg(&inv),
        )
        .await?;
        Self::simple_event(
            &mut tx,
            "org_invitation.created",
            org_id,
            serde_json::to_string(&inv).unwrap_or_default(),
        )
        .await?;
        tx.audit(
            AuditDraft::new("org_invitation.create", org_id)
                .target_id(&inv.id)
                .after(&serde_json::to_string(&inv).unwrap_or_default()),
        )
        .await?;
        tx.commit().await?;
        Ok(inv)
    }

    fn with_expiry(mut inv: OrgInvitation, row: &Row, now: u64) -> OrgInvitation {
        if inv.state == InvitationState::Pending as i32 && row.n1.unwrap_or(0) as u64 <= now {
            inv.state = InvitationState::Expired as i32;
        }
        inv
    }

    /// An org's invitations, optionally of one state.
    pub async fn invitations(
        &self,
        cx: &Cx,
        org_id: &str,
        state: InvitationState,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<OrgInvitation>, String)> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let now = tx.now_ms();
        let rows = tx
            .scan_msgs::<OrgInvitation>(
                &ORG_INVITES,
                &Scan::scope("", crate::store::MAX_SCAN)
                    .key(Key::K1(org_id.into()))
                    .after(page_cursor("", after)),
            )
            .await?;
        let all: Vec<OrgInvitation> = rows
            .into_iter()
            .map(|(i, r)| Self::with_expiry(i, &r, now))
            .filter(|i| state == InvitationState::Unspecified || i.state == state as i32)
            .collect();
        let more = all.len() > limit;
        let out: Vec<OrgInvitation> = all.into_iter().take(limit).collect();
        let next = next_token(&out, more, |i| i.id.clone());
        Ok((out, next))
    }

    /// One invitation (any org).
    pub async fn invitation(&self, cx: &Cx, id: &str) -> Result<OrgInvitation> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let now = tx.now_ms();
        match tx.get_msg::<OrgInvitation>(&ORG_INVITES, "", id).await? {
            Some((i, r)) => Ok(Self::with_expiry(i, &r, now)),
            None => Err(ForgeError::not_found(format!("invitation {id}"))),
        }
    }

    async fn close_invitation(
        &self,
        cx: &Cx,
        id: &str,
        org_check: Option<&str>,
        caller: Option<&Caller>,
        to: InvitationState,
    ) -> Result<OrgInvitation> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let now = tx.now_ms();
        let Some((inv, row)) = tx.get_msg::<OrgInvitation>(&ORG_INVITES, "", id).await? else {
            return Err(ForgeError::not_found(format!("invitation {id}")));
        };
        if org_check.is_some_and(|o| o != inv.org_id) {
            return Err(ForgeError::not_found(format!("invitation {id}")));
        }
        if let Some(c) = caller
            && !c.matches(row.k2.as_deref().unwrap_or(""))
        {
            return Err(ForgeError::not_found(format!("invitation {id}")));
        }
        let mut inv = Self::with_expiry(inv, &row, now);
        if inv.state != InvitationState::Pending as i32 {
            return Err(ForgeError::invalid_state(format!(
                "invitation is {}",
                InvitationState::try_from(inv.state)
                    .unwrap_or(InvitationState::Unspecified)
                    .as_str_name()
            )));
        }
        inv.state = to as i32;
        tx.update(&ORG_INVITES, &row.clone().msg(&inv)).await?;
        if to == InvitationState::Accepted {
            let c = caller.ok_or_else(|| ForgeError::internal("accept without caller"))?;
            let role = Role::try_from(inv.role).unwrap_or(Role::Member);
            if tx
                .get(&MEMBERS, &inv.org_id, &c.principal.id)
                .await?
                .is_none()
            {
                let m = Member {
                    principal: Some(c.principal.clone()),
                    role: role as i32,
                    joined_at: Some(ts(now)),
                    ..Default::default()
                };
                tx.insert(
                    &MEMBERS,
                    &Row::new(inv.org_id.clone(), c.principal.id.clone())
                        .k1(org_role_name(role))
                        .k2(c.principal.id.clone())
                        .msg(&m),
                )
                .await?;
                Self::simple_event(
                    &mut tx,
                    "member.added",
                    &inv.org_id,
                    serde_json::json!({"principal_id": c.principal.id}).to_string(),
                )
                .await?;
            }
        }
        let (kind, action) = match to {
            InvitationState::Accepted => ("org_invitation.accepted", "org_invitation.accept"),
            InvitationState::Declined => ("org_invitation.declined", "org_invitation.decline"),
            _ => ("org_invitation.cancelled", "org_invitation.cancel"),
        };
        Self::simple_event(
            &mut tx,
            kind,
            &inv.org_id,
            serde_json::to_string(&inv).unwrap_or_default(),
        )
        .await?;
        tx.audit(AuditDraft::new(action, &inv.org_id).target_id(id))
            .await?;
        tx.commit().await?;
        Ok(inv)
    }

    /// Cancels a pending invitation of `org_id`.
    pub async fn cancel_invitation(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        self.close_invitation(cx, id, Some(org_id), None, InvitationState::Cancelled)
            .await
            .map(|_| ())
    }

    /// Accepts an invitation addressed to `caller`; returns the membership.
    pub async fn accept_invitation(&self, cx: &Cx, id: &str, caller: &Caller) -> Result<Member> {
        let inv = self
            .close_invitation(cx, id, None, Some(caller), InvitationState::Accepted)
            .await?;
        self.member(cx, &inv.org_id, &caller.principal.id)
            .await?
            .ok_or_else(|| ForgeError::internal("membership missing after acceptance"))
    }

    /// Declines an invitation addressed to `caller`.
    pub async fn decline_invitation(&self, cx: &Cx, id: &str, caller: &Caller) -> Result<()> {
        self.close_invitation(cx, id, None, Some(caller), InvitationState::Declined)
            .await
            .map(|_| ())
    }

    /// Principals with direct grants on org repos who are not members, with those repos.
    pub async fn outside_collaborators(
        &self,
        cx: &Cx,
        org_id: &str,
    ) -> Result<Vec<OutsideCollaborator>> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let repos = tx
            .scan(
                &REPOS,
                &Scan::scope("", crate::store::MAX_SCAN).key(Key::K1(org_id.into())),
            )
            .await?;
        let mut by: std::collections::BTreeMap<String, OutsideCollaborator> = Default::default();
        for repo in repos {
            if repo.n1.is_some() {
                continue;
            }
            let collabs = tx
                .scan_msgs::<bee_proto::v1::Collaborator>(
                    &COLLABORATORS,
                    &Scan::scope(repo.id.clone(), crate::store::MAX_SCAN),
                )
                .await?;
            for (c, r) in collabs {
                if tx.get(&MEMBERS, org_id, &r.id).await?.is_some() {
                    continue;
                }
                let e = by
                    .entry(r.id.clone())
                    .or_insert_with(|| OutsideCollaborator {
                        principal: c.principal.clone(),
                        repo_ids: Vec::new(),
                    });
                e.repo_ids.push(repo.id.clone());
            }
        }
        Ok(by.into_values().collect())
    }

    /// Turns a member into an outside collaborator: the membership goes, direct repo grants
    /// stay.
    pub async fn convert_to_outside(
        &self,
        cx: &Cx,
        org_id: &str,
        principal_id: &str,
    ) -> Result<OutsideCollaborator> {
        let m = self
            .member(cx, org_id, principal_id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("member {principal_id}")))?;
        self.set_member(
            cx,
            org_id,
            m.principal.clone().unwrap_or_default(),
            Role::Unspecified,
        )
        .await?;
        let all = self.outside_collaborators(cx, org_id).await?;
        Ok(all
            .into_iter()
            .find(|o| o.principal.as_ref().is_some_and(|p| p.id == principal_id))
            .unwrap_or(OutsideCollaborator {
                principal: m.principal,
                repo_ids: Vec::new(),
            }))
    }

    /// Custom property definitions of an org.
    pub async fn property_defs(
        &self,
        cx: &Cx,
        org_id: &str,
    ) -> Result<Vec<CustomPropertyDefinition>> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        Ok(tx
            .scan_msgs::<CustomPropertyDefinition>(&PROP_DEFS, &Scan::scope(org_id, 1000))
            .await?
            .into_iter()
            .map(|(d, _)| d)
            .collect())
    }

    /// Creates or replaces a definition by name.
    pub async fn set_property_def(
        &self,
        cx: &Cx,
        org_id: &str,
        def: CustomPropertyDefinition,
    ) -> Result<CustomPropertyDefinition> {
        validate_property_name(&def.name)?;
        let ty =
            CustomPropertyType::try_from(def.r#type).unwrap_or(CustomPropertyType::Unspecified);
        if ty == CustomPropertyType::Unspecified {
            return Err(ForgeError::missing("definition.type"));
        }
        if matches!(
            ty,
            CustomPropertyType::SingleSelect | CustomPropertyType::MultiSelect
        ) && def.allowed_values.is_empty()
        {
            return Err(ForgeError::invalid(
                "missing_allowed_values",
                "select properties need allowed_values",
            ));
        }
        if !def.default_value.is_empty() {
            check_property_value(&def, &def.default_value)?;
        }
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        tx.put(&PROP_DEFS, &Row::new(org_id, def.name.clone()).msg(&def))
            .await?;
        let after = serde_json::to_string(&def).unwrap_or_default();
        Self::simple_event(&mut tx, "custom_property.updated", org_id, after.clone()).await?;
        tx.audit(
            AuditDraft::new("custom_property.set", org_id)
                .target_id(&def.name)
                .after(&after),
        )
        .await?;
        tx.commit().await?;
        Ok(def)
    }

    /// Deletes a definition.
    pub async fn delete_property_def(&self, cx: &Cx, org_id: &str, name: &str) -> Result<()> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        if !tx.delete(&PROP_DEFS, org_id, name).await? {
            return Err(ForgeError::not_found(format!("custom property {name}")));
        }
        Self::simple_event(
            &mut tx,
            "custom_property.updated",
            org_id,
            serde_json::json!({"deleted": name}).to_string(),
        )
        .await?;
        tx.audit(AuditDraft::new("custom_property.delete", org_id).target_id(name))
            .await?;
        tx.commit().await
    }

    /// Hard-deletes orgs and repos whose restore window passed and expired redirects;
    /// returns how many rows went.
    pub async fn purge_expired(&self, cx: &Cx) -> Result<usize> {
        let mut tx = self.db.begin(Surface::ORGS, cx).await?;
        let cutoff = tx.now_ms().saturating_sub(RESTORE_DAYS * DAY_MS) as i64;
        let now = tx.now_ms() as i64;
        let mut n = 0;
        for t in [&ORGS, &REPOS] {
            let rows = tx
                .scan(
                    t,
                    &Scan {
                        order: crate::store::Order::N1,
                        n1_max: Some(cutoff),
                        limit: 1000,
                        ..Scan::default()
                    },
                )
                .await?;
            for r in rows {
                tx.delete(t, &r.scope, &r.id).await?;
                n += 1;
            }
        }
        for t in [&ORG_REDIRECTS, &REPO_REDIRECTS] {
            let rows = tx
                .scan(
                    t,
                    &Scan {
                        order: crate::store::Order::N1,
                        n1_max: Some(now),
                        limit: 1000,
                        ..Scan::default()
                    },
                )
                .await?;
            for r in rows {
                tx.delete(t, &r.scope, &r.id).await?;
                n += 1;
            }
        }
        tx.commit().await?;
        Ok(n)
    }
}

fn validate_property_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 75
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ForgeError::invalid(
            "invalid_name",
            "property names are 1-75 of A-Z a-z 0-9 _ -",
        ));
    }
    Ok(())
}

/// Checks a value against a definition (multi-select values are comma-separated).
pub fn check_property_value(def: &CustomPropertyDefinition, value: &str) -> Result<()> {
    let bad = |m: String| Err(ForgeError::invalid("invalid_property_value", m));
    match CustomPropertyType::try_from(def.r#type).unwrap_or(CustomPropertyType::Unspecified) {
        CustomPropertyType::TrueFalse if value != "true" && value != "false" => {
            bad(format!("{} must be true or false", def.name))
        }
        CustomPropertyType::SingleSelect if !def.allowed_values.iter().any(|v| v == value) => {
            bad(format!("{value:?} is not an allowed value of {}", def.name))
        }
        CustomPropertyType::MultiSelect => {
            for v in value.split(',') {
                if !def.allowed_values.iter().any(|a| a == v) {
                    return bad(format!("{v:?} is not an allowed value of {}", def.name));
                }
            }
            Ok(())
        }
        CustomPropertyType::String if value.len() > 1000 => {
            bad(format!("{} is too long", def.name))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("beehive_1.x").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name(&"a".repeat(101)).is_err());
    }

    #[test]
    fn property_values() {
        let d = CustomPropertyDefinition {
            name: "tier".into(),
            r#type: CustomPropertyType::MultiSelect as i32,
            allowed_values: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        assert!(check_property_value(&d, "a,b").is_ok());
        assert!(check_property_value(&d, "a,c").is_err());
    }
}
