//! Users and profiles (PRODUCT §4.16, F416-039..049): profile, status, pinned items,
//! followers, stars and star lists, emails with noreply addresses and commit-email mapping,
//! account settings, data-export requests, username changes with redirects and account
//! deletion. Authentication, sessions and keys belong to the identity layer.
//!
//! # Identity
//!
//! A principal gets a profile the first time anything asks for it ([`Users::ensure`]); its
//! login comes from the principal directory (first login) and is unique case-insensitively.
//! **Rename** moves the login and keeps the old name as a redirect for 90 days
//! ([`REDIRECT_DAYS`]); nobody else can claim a redirecting name, but its owner can take it
//! back. [`Users::resolve_login`] follows redirects. **Deletion** hides the profile at once,
//! reserves the login and sets a purge time 90 days out ([`PURGE_DAYS`]) before which support
//! can restore it.
//!
//! # Emails
//!
//! An added address is unverified until its token is redeemed ([`Users::verify_email`]);
//! tokens are queued for the mailer ([`Users::claim_verifications`]). A verified address is
//! unique across accounts and maps commits to the account
//! ([`Users::resolve_commit_emails`]). Every account also has a generated noreply address
//! `<id>+<login>@<noreply domain>` that always resolves to it. Imported identities are
//! *placeholders*: an address registered with [`Users::register_placeholder`] resolves to the
//! placeholder until a real account verifies that address and claims it.
//!
//! # Tables (surface `users`)
//!
//! | table | scope | id | keys |
//! |---|---|---|---|
//! | profiles | `""` | principal | `k1` login (unique) |
//! | logins | `""` | login | `k1` principal, `n1` redirect expiry (absent = current) |
//! | emails | principal | address | `k1` address, `k2` `v`/`u`, `n1` added |
//! | follows | follower | followee | `k1` followee, `n1` time |
//! | stars | principal | repo id | `k1` repo id, `n1` time |
//! | repo follows | follower (human or agent) | repo id | `k1` repo id, `n1` time |
//! | star lists, items | principal / list | ULID / repo id | |
//! | security events | principal | ULID | `k1` action, `n1` time |

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    KeyValue, Principal, PrincipalKind, Repo, StarList, SubjectKind, UserCommitEmailMapping,
    UserDataExport, UserDataExportState, UserEmail, UserPinnedItemRef, UserProfile,
    UserSecurityEvent, UserSettings, UserSocialAccount, UserStatus, UserSummary,
};
use prost::Message;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::hooks::PrincipalDirectory;
use crate::ids::{hex, ts, ts_ms};
use crate::module::EventKind;
use crate::store::{Db, Index, Key, Order, Row, Scan, Surface, TableDef, Tx};

use super::common::{draft, emit, max_len, page_vec, repo_ref_id, required, subject};
use super::notifications::EmailDirectory;
use super::ports::{SignedUrl, UrlSigner};
use super::tz::Zone;

/// The users surface.
pub const USERS: Surface = Surface("users");

/// Days a renamed login keeps redirecting.
pub const REDIRECT_DAYS: u64 = 90;
/// Days before a deleted account is purged.
pub const PURGE_DAYS: u64 = 90;
/// Most pinned items.
pub const MAX_PINNED: usize = 6;
/// Most star lists per user.
pub const MAX_STAR_LISTS: usize = 32;
/// Most emails per account.
pub const MAX_EMAILS: usize = 20;

const DAY_MS: u64 = 86_400_000;

/// Profiles.
pub static PROFILES: TableDef = TableDef {
    name: "forge_user_profiles",
    surface: USERS,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};
/// Logins and their redirects.
pub static LOGINS: TableDef = TableDef {
    name: "forge_user_logins",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1],
};
/// Emails.
pub static EMAILS: TableDef = TableDef {
    name: "forge_user_emails",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::K1, Index::ScopeN1],
};
/// Email verification tokens (also the mailer's queue).
pub static EMAIL_TOKENS: TableDef = TableDef {
    name: "forge_user_email_tokens",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};
/// Placeholder identities of imported commit emails.
pub static PLACEHOLDERS: TableDef = TableDef {
    name: "forge_user_placeholders",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Follows.
pub static FOLLOWS: TableDef = TableDef {
    name: "forge_user_follows",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::K1, Index::N1, Index::ScopeN1],
};
/// Repo stars.
pub static STARS: TableDef = TableDef {
    name: "forge_user_stars",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::K1, Index::N1, Index::ScopeN1],
};
/// Repo follows: a follower's Following feed reads the posts of the repos it follows.
pub static REPO_FOLLOWS: TableDef = TableDef {
    name: "forge_repo_follows",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::K1, Index::N1, Index::ScopeN1],
};
/// Star lists.
pub static STAR_LISTS: TableDef = TableDef {
    name: "forge_user_star_lists",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};
/// Star list members.
pub static STAR_LIST_ITEMS: TableDef = TableDef {
    name: "forge_user_star_list_items",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};
/// Counters: followers, following, stars (per principal), stargazers (per repo).
pub static COUNTERS: TableDef = TableDef {
    name: "forge_user_counters",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Data export requests.
pub static EXPORTS: TableDef = TableDef {
    name: "forge_user_exports",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1, Index::N1],
};
/// Per-user security log.
pub static SECURITY_EVENTS: TableDef = TableDef {
    name: "forge_user_security_events",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::ScopeN1],
};
/// Pending avatar uploads.
pub static AVATAR_UPLOADS: TableDef = TableDef {
    name: "forge_user_avatar_uploads",
    surface: USERS,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// User event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::new("user.profile_updated", "A profile changed"),
    EventKind::new("user.status_changed", "A status was set or cleared"),
    EventKind::new("user.avatar_updated", "An avatar was uploaded"),
    EventKind::new("user.pinned_items_updated", "Pinned items changed"),
    EventKind::new(
        "user.email_changed",
        "An email was added, verified, removed or changed",
    ),
    EventKind::new("user.settings_updated", "Account settings changed"),
    EventKind::new("user.login_renamed", "A login was renamed"),
    EventKind::new("user.export_requested", "A data export was requested"),
    EventKind::new("user.deleted", "An account was deleted"),
    EventKind::new("user.restored", "A deleted account was restored"),
    EventKind::new("follow.added", "A user followed another"),
    EventKind::new("follow.removed", "A user unfollowed another"),
    EventKind::new("follow.repo_added", "A user or agent followed a repo"),
    EventKind::new(
        "follow.repo_removed",
        "A user or agent stopped following a repo",
    ),
    EventKind::gh("star.added", "A repo was starred", "watch", "started"),
    EventKind::new("star.removed", "A repo was unstarred"),
    EventKind::new(
        "star_list.changed",
        "A star list was created, changed or deleted",
    ),
];

/// A repo's creator follows it: on `repo.created`, the principal that created it (a person
/// or an agent; never the server itself) follows the repo, so the posts of their own repos
/// reach their Following feed. Registered on the event log by [`crate::forge::Forge::open`].
pub struct AutoFollow {
    users: Arc<Users>,
    orgs: Arc<crate::orgs::Orgs>,
}

impl AutoFollow {
    /// The listener over the users store and the orgs.
    pub fn new(users: Arc<Users>, orgs: Arc<crate::orgs::Orgs>) -> AutoFollow {
        AutoFollow { users, orgs }
    }
}

#[async_trait]
impl crate::events::EventListener for AutoFollow {
    async fn on_event(&self, event: &bee_proto::v1::Event) {
        if event.kind != "repo.created"
            || event.principal.is_empty()
            || event.principal == "system"
            || event.principal == crate::auth::ANONYMOUS_PRINCIPAL
        {
            return;
        }
        let cx = Cx::system();
        let Ok(repo) = self.orgs.repo_any(&cx, &event.repo_id).await else {
            return;
        };
        let _ = self
            .users
            .follow_repo_as(&cx, &event.principal, &repo)
            .await;
    }
}

/// Settings of the users module.
#[derive(Clone, Debug)]
pub struct UsersConfig {
    /// Domain of generated noreply addresses.
    pub noreply_domain: String,
    /// Base URL for profile links.
    pub base_url: String,
}

impl Default for UsersConfig {
    fn default() -> UsersConfig {
        UsersConfig {
            noreply_domain: "users.noreply.beehive.invalid".into(),
            base_url: "https://beehive.invalid".into(),
        }
    }
}

/// A profile's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct ProfileRec {
    /// The profile fields (counts are computed on read).
    #[prost(message, optional, tag = "1")]
    pub profile: Option<UserProfile>,
    /// Account settings.
    #[prost(message, optional, tag = "2")]
    pub settings: Option<UserSettings>,
    /// Pinned items in display order.
    #[prost(message, repeated, tag = "3")]
    pub pinned: Vec<UserPinnedItemRef>,
    /// Use the noreply address for web operations.
    #[prost(bool, tag = "4")]
    pub keep_private: bool,
    /// Reject writes authored with a private email.
    #[prost(bool, tag = "5")]
    pub block_private_pushes: bool,
    /// When the account was deleted (ms), 0 = live.
    #[prost(uint64, tag = "6")]
    pub deleted_ms: u64,
    /// Content id of the avatar.
    #[prost(string, tag = "7")]
    pub avatar_sha: String,
}

/// An email's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct EmailRec {
    /// The email.
    #[prost(message, optional, tag = "1")]
    pub email: Option<UserEmail>,
}

/// A pending verification.
#[derive(Clone, PartialEq, Message)]
pub struct VerifyRec {
    /// Principal.
    #[prost(string, tag = "1")]
    pub principal_id: String,
    /// Address to verify.
    #[prost(string, tag = "2")]
    pub email: String,
    /// Expiry (ms).
    #[prost(uint64, tag = "3")]
    pub expires_ms: u64,
    /// The mailer took it.
    #[prost(bool, tag = "4")]
    pub sent: bool,
    /// The token (also the row id).
    #[prost(string, tag = "5")]
    pub token: String,
}

/// A data export's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct ExportRec {
    /// The export as returned (without the URL).
    #[prost(message, optional, tag = "1")]
    pub export: Option<UserDataExport>,
    /// Include repo contents.
    #[prost(bool, tag = "2")]
    pub include_repos: bool,
    /// Include attachments.
    #[prost(bool, tag = "3")]
    pub include_attachments: bool,
    /// Blob-store key of the archive.
    #[prost(string, tag = "4")]
    pub key: String,
}

/// A pending avatar upload.
#[derive(Clone, PartialEq, Message)]
pub struct AvatarUpload {
    /// Blob-store key.
    #[prost(string, tag = "1")]
    pub key: String,
    /// Content type.
    #[prost(string, tag = "2")]
    pub content_type: String,
    /// Declared size.
    #[prost(uint64, tag = "3")]
    pub size: u64,
    /// Expiry (ms).
    #[prost(uint64, tag = "4")]
    pub expires_ms: u64,
}

/// A partial profile update.
#[derive(Clone, Debug, Default)]
pub struct ProfilePatch {
    /// Display name.
    pub name: Option<String>,
    /// Bio.
    pub bio: Option<String>,
    /// Pronouns.
    pub pronouns: Option<String>,
    /// Company.
    pub company: Option<String>,
    /// Location.
    pub location: Option<String>,
    /// Website.
    pub website: Option<String>,
    /// Social accounts, replacing all when `replace_social`.
    pub social: Vec<UserSocialAccount>,
    /// Replace the social accounts.
    pub replace_social: bool,
    /// Public email (must be verified; empty hides).
    pub public_email: Option<String>,
    /// Open to work.
    pub hireable: Option<bool>,
}

/// A user's settings patch.
#[derive(Clone, Debug, Default)]
pub struct SettingsPatch {
    /// Theme.
    pub theme: Option<String>,
    /// Dark theme.
    pub dark_theme: Option<String>,
    /// Light theme.
    pub light_theme: Option<String>,
    /// Time zone.
    pub time_zone: Option<String>,
    /// Locale.
    pub locale: Option<String>,
    /// Accessibility flags merged by key (empty value removes).
    pub accessibility: Vec<KeyValue>,
    /// Shortcuts merged by key.
    pub keyboard_shortcuts: Vec<KeyValue>,
    /// Shortcuts on or off.
    pub keyboard_shortcuts_enabled: Option<bool>,
    /// Editor indent.
    pub editor_indent: Option<String>,
    /// Diff view.
    pub diff_view: Option<String>,
    /// Count private contributions.
    pub show_private_contributions: Option<bool>,
    /// Flag unsigned changesets.
    pub vigilant_mode: Option<bool>,
}

/// Validates a login: 1 to 39 letters, digits and single dashes, not starting or ending with
/// a dash.
pub fn validate_login(login: &str) -> Result<()> {
    let ok = !login.is_empty()
        && login.len() <= 39
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(ForgeError::invalid(
            "invalid_login",
            format!("{login:?} is not a valid login (1-39 letters, digits and single dashes)"),
        ))
    }
}

/// Validates an email address (one `@`, non-empty parts, no spaces).
pub fn validate_email(email: &str) -> Result<String> {
    let e = email.trim().to_lowercase();
    let ok = e.len() <= 254
        && e.split_once('@').is_some_and(|(l, d)| {
            !l.is_empty()
                && d.contains('.')
                && !d.starts_with('.')
                && !d.ends_with('.')
                && !d.contains('@')
        })
        && !e.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(e)
    } else {
        Err(ForgeError::invalid(
            "invalid_email",
            format!("{email:?} is not a valid email address"),
        ))
    }
}

fn valid_url(u: &str) -> bool {
    (u.starts_with("https://") || u.starts_with("http://"))
        && u.len() <= 255
        && !u.contains(char::is_whitespace)
}

fn default_user_settings() -> UserSettings {
    UserSettings {
        theme: "system".into(),
        time_zone: "UTC".into(),
        locale: "en".into(),
        keyboard_shortcuts_enabled: true,
        editor_indent: "spaces:2".into(),
        diff_view: "unified".into(),
        ..Default::default()
    }
}

/// Users over the store.
pub struct Users {
    db: Db,
    directory: Arc<dyn PrincipalDirectory>,
    signer: Arc<dyn UrlSigner>,
    cfg: UsersConfig,
}

impl Users {
    /// Users over `db`.
    pub fn new(
        db: Db,
        directory: Arc<dyn PrincipalDirectory>,
        signer: Arc<dyn UrlSigner>,
        cfg: UsersConfig,
    ) -> Users {
        Users {
            db,
            directory,
            signer,
            cfg,
        }
    }

    fn url(&self, login: &str) -> String {
        format!("{}/{login}", self.cfg.base_url.trim_end_matches('/'))
    }

    async fn counter(tx: &mut Tx, scope: &str, name: &str) -> Result<u64> {
        Ok(tx
            .get(&COUNTERS, scope, name)
            .await?
            .and_then(|r| r.n1)
            .unwrap_or(0)
            .max(0) as u64)
    }

    async fn security(
        tx: &mut Tx,
        principal: &str,
        cx: &Cx,
        action: &str,
        details: serde_json::Value,
    ) -> Result<()> {
        let id = tx.new_id()?;
        let ev = UserSecurityEvent {
            id: id.clone(),
            action: action.to_string(),
            actor: Some(cx.principal()),
            ip_address: cx.ip.clone(),
            user_agent: cx.user_agent.clone(),
            location: String::new(),
            details: details.to_string(),
            created_at: Some(ts(tx.now_ms())),
        };
        tx.insert(
            &SECURITY_EVENTS,
            &Row::new(principal, id)
                .k1(action.to_string())
                .n1(tx.now_ms() as i64)
                .msg(&ev),
        )
        .await
    }

    async fn announce(
        tx: &mut Tx,
        kind: &str,
        audit: &str,
        principal: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        emit(
            tx,
            kind,
            "",
            "",
            subject(SubjectKind::User, "", principal),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(audit, "", "", SubjectKind::User, principal, &payload))
            .await
    }

    // ---- identity ---------------------------------------------------------------------

    /// The profile record of a principal, created from the directory on first use.
    pub async fn ensure(&self, cx: &Cx, principal_id: &str) -> Result<(ProfileRec, Row)> {
        {
            let mut tx = self.db.begin(USERS, cx).await?;
            if let Some(row) = tx.get(&PROFILES, "", principal_id).await? {
                return Ok((row.decode()?, row));
            }
        }
        let p = self
            .directory
            .get(principal_id)
            .await
            .ok_or_else(|| ForgeError::not_found(format!("user {principal_id}")))?;
        let (logins, _) = self.directory.identities(principal_id).await;
        let mut tx = self.db.begin(USERS, cx).await?;
        if let Some(row) = tx.get(&PROFILES, "", principal_id).await? {
            return Ok((row.decode()?, row));
        }
        let now = tx.now_ms();
        let mut login = logins
            .into_iter()
            .find(|l| validate_login(l).is_ok())
            .unwrap_or_else(|| format!("u-{}", principal_id.to_lowercase()));
        if tx.get(&LOGINS, "", &login.to_lowercase()).await?.is_some() {
            login = format!("u-{}", principal_id.to_lowercase());
        }
        let rec = ProfileRec {
            profile: Some(UserProfile {
                principal: Some(p),
                login: login.clone(),
                created_at: Some(ts(now)),
                updated_at: Some(ts(now)),
                ..Default::default()
            }),
            settings: Some(default_user_settings()),
            ..Default::default()
        };
        let row = Row::new("", principal_id)
            .k1(login.to_lowercase())
            .msg(&rec);
        tx.insert(&PROFILES, &row).await?;
        tx.insert(
            &LOGINS,
            &Row::new("", login.to_lowercase()).k1(principal_id),
        )
        .await?;
        tx.commit().await?;
        Ok((rec, row.version(1)))
    }

    /// Resolves a login (following redirects) to `(principal id, current login, redirected)`.
    pub async fn resolve_login(&self, cx: &Cx, login: &str) -> Result<(String, String, bool)> {
        let key = login.to_lowercase();
        let now = self.db.now_ms();
        let hit = {
            let mut tx = self.db.begin(USERS, cx).await?;
            tx.get(&LOGINS, "", &key).await?
        };
        let (pid, redirected) = match hit {
            Some(r) if r.n1.is_none() => (r.k1.unwrap_or_default(), false),
            Some(r) if r.n1.unwrap_or(0) as u64 > now => (r.k1.unwrap_or_default(), true),
            _ => match self.directory.by_login(login).await {
                Some(p) => (p.id, false),
                None => return Err(ForgeError::not_found(format!("user {login}"))),
            },
        };
        let (rec, _) = self.ensure(cx, &pid).await?;
        if rec.deleted_ms > 0 {
            return Err(ForgeError::not_found(format!("user {login}")));
        }
        let current = rec.profile.map(|p| p.login).unwrap_or_default();
        Ok((pid, current, redirected))
    }

    /// A user summary, or `None` for an unknown or deleted user.
    pub async fn summary(&self, cx: &Cx, principal_id: &str) -> Option<UserSummary> {
        let (rec, _) = self.ensure(cx, principal_id).await.ok()?;
        if rec.deleted_ms > 0 {
            return None;
        }
        let p = rec.profile?;
        Some(UserSummary {
            principal: p.principal,
            login: p.login,
            name: p.name,
            avatar_url: p.avatar_url,
            bio: p.bio,
        })
    }

    /// A profile as `viewer` sees it. Repo and gist counts are filled by the caller.
    pub async fn profile(&self, cx: &Cx, principal_id: &str, viewer: &str) -> Result<UserProfile> {
        let (rec, _) = self.ensure(cx, principal_id).await?;
        if rec.deleted_ms > 0 {
            return Err(ForgeError::not_found(format!("user {principal_id}")));
        }
        let mut p = rec.profile.clone().unwrap_or_default();
        let mut tx = self.db.begin(USERS, cx).await?;
        p.follower_count = Self::counter(&mut tx, principal_id, "followers").await?;
        p.following_count = Self::counter(&mut tx, principal_id, "following").await?;
        p.star_count = Self::counter(&mut tx, principal_id, "stars").await?;
        if !viewer.is_empty() && viewer != principal_id {
            p.viewer_is_following = tx.get(&FOLLOWS, viewer, principal_id).await?.is_some();
            p.follows_viewer = tx.get(&FOLLOWS, principal_id, viewer).await?.is_some();
        }
        if let Some(st) = &p.status
            && st
                .expires_at
                .as_ref()
                .is_some_and(|e| ts_ms(e) <= tx.now_ms())
        {
            p.status = None;
        }
        p.url = self.url(&p.login);
        if !p.public_email.is_empty() && principal_id != viewer {
            // Only addresses the owner marked public are ever stored here.
        }
        Ok(p)
    }

    /// Applies a profile patch.
    pub async fn update_profile(&self, cx: &Cx, patch: ProfilePatch) -> Result<UserProfile> {
        self.ensure(cx, &cx.actor_id).await?;
        for (f, v, max) in [
            ("name", &patch.name, 100usize),
            ("bio", &patch.bio, 500),
            ("pronouns", &patch.pronouns, 40),
            ("company", &patch.company, 100),
            ("location", &patch.location, 100),
        ] {
            if let Some(v) = v {
                max_len(f, v, max)?;
            }
        }
        if let Some(w) = &patch.website
            && !w.is_empty()
            && !valid_url(w)
        {
            return Err(ForgeError::invalid(
                "bad_url",
                "the website must be an http(s) URL",
            ));
        }
        if patch.social.len() > 4 {
            return Err(ForgeError::too_large(
                "social_accounts",
                4,
                patch.social.len() as u64,
            ));
        }
        for s in &patch.social {
            if s.provider.is_empty() || !valid_url(&s.url) {
                return Err(ForgeError::invalid(
                    "bad_social_account",
                    "a social account needs a provider and an http(s) URL",
                ));
            }
        }
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some(mut row) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? else {
            return Err(ForgeError::not_found("profile"));
        };
        let mut rec: ProfileRec = row.decode()?;
        let mut p = rec.profile.clone().unwrap_or_default();
        if let Some(v) = patch.name {
            p.name = v;
        }
        if let Some(v) = patch.bio {
            p.bio = v;
        }
        if let Some(v) = patch.pronouns {
            p.pronouns = v;
        }
        if let Some(v) = patch.company {
            p.company = v;
        }
        if let Some(v) = patch.location {
            p.location = v;
        }
        if let Some(v) = patch.website {
            p.website = v;
        }
        if patch.replace_social {
            p.social_accounts = patch.social;
        }
        if let Some(v) = patch.hireable {
            p.hireable = v;
        }
        if let Some(e) = patch.public_email {
            if e.is_empty() {
                p.public_email.clear();
            } else {
                let e = validate_email(&e)?;
                match tx.get_msg::<EmailRec>(&EMAILS, &cx.actor_id, &e).await? {
                    Some((r, _)) if r.email.as_ref().is_some_and(|x| x.verified) => {
                        p.public_email = e
                    }
                    _ => {
                        return Err(ForgeError::invalid(
                            "unverified_email",
                            "the public email must be one of your verified addresses",
                        ));
                    }
                }
            }
        }
        p.updated_at = Some(ts(tx.now_ms()));
        rec.profile = Some(p.clone());
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        Self::announce(
            &mut tx,
            "user.profile_updated",
            "user.profile_update",
            &cx.actor_id,
            serde_json::json!({"login": p.login}),
        )
        .await?;
        tx.commit().await?;
        self.profile(cx, &cx.actor_id, &cx.actor_id).await
    }

    /// Starts an avatar upload (PNG, JPEG or GIF up to 1 MiB).
    pub async fn begin_avatar(
        &self,
        cx: &Cx,
        content_type: &str,
        size: u64,
    ) -> Result<(String, SignedUrl)> {
        if !["image/png", "image/jpeg", "image/gif"].contains(&content_type) {
            return Err(ForgeError::invalid(
                "bad_content_type",
                "avatars are image/png, image/jpeg or image/gif",
            ));
        }
        if size == 0 || size > (1 << 20) {
            return Err(ForgeError::too_large("avatar_bytes", 1 << 20, size));
        }
        self.ensure(cx, &cx.actor_id).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let id = tx.new_id()?;
        let now = tx.now_ms();
        let key = format!("avatars/{}/{id}", cx.actor_id);
        let signed = self.signer.upload(&key, size, content_type, now);
        tx.insert(
            &AVATAR_UPLOADS,
            &Row::new(cx.actor_id.clone(), id.clone()).msg(&AvatarUpload {
                key,
                content_type: content_type.into(),
                size,
                expires_ms: signed.expires_ms,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok((id, signed))
    }

    /// Completes an avatar upload with the SHA-256 of the bytes.
    pub async fn finalize_avatar(
        &self,
        cx: &Cx,
        upload_id: &str,
        sha256: &[u8],
    ) -> Result<UserProfile> {
        if sha256.len() != 32 {
            return Err(ForgeError::invalid("bad_digest", "sha256 must be 32 bytes"));
        }
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some((up, _)) = tx
            .get_msg::<AvatarUpload>(&AVATAR_UPLOADS, &cx.actor_id, upload_id)
            .await?
        else {
            return Err(ForgeError::not_found(format!("avatar upload {upload_id}")));
        };
        if up.expires_ms <= tx.now_ms() {
            return Err(ForgeError::invalid_state("the avatar upload expired")
                .with_subcode("upload_expired"));
        }
        let Some(mut row) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? else {
            return Err(ForgeError::not_found("profile"));
        };
        let mut rec: ProfileRec = row.decode()?;
        let mut p = rec.profile.clone().unwrap_or_default();
        p.avatar_url = self.signer.public(&up.key);
        p.updated_at = Some(ts(tx.now_ms()));
        rec.avatar_sha = hex(sha256);
        rec.profile = Some(p);
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        tx.delete(&AVATAR_UPLOADS, &cx.actor_id, upload_id).await?;
        Self::announce(
            &mut tx,
            "user.avatar_updated",
            "user.avatar_update",
            &cx.actor_id,
            serde_json::json!({"blob": hex(sha256)}),
        )
        .await?;
        tx.commit().await?;
        self.profile(cx, &cx.actor_id, &cx.actor_id).await
    }

    /// Sets the caller's status.
    pub async fn set_status(&self, cx: &Cx, mut st: UserStatus) -> Result<UserStatus> {
        max_len("message", &st.message, 80)?;
        max_len("emoji", &st.emoji, 40)?;
        self.ensure(cx, &cx.actor_id).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let now = tx.now_ms();
        if st.expires_at.as_ref().is_some_and(|e| ts_ms(e) <= now) {
            return Err(ForgeError::invalid(
                "expired_status",
                "the status already expired",
            ));
        }
        st.updated_at = Some(ts(now));
        self.write_status(&mut tx, cx, Some(st.clone())).await?;
        tx.commit().await?;
        Ok(st)
    }

    /// Clears the caller's status.
    pub async fn clear_status(&self, cx: &Cx) -> Result<()> {
        self.ensure(cx, &cx.actor_id).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        self.write_status(&mut tx, cx, None).await?;
        tx.commit().await
    }

    async fn write_status(&self, tx: &mut Tx, cx: &Cx, st: Option<UserStatus>) -> Result<()> {
        let Some(mut row) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? else {
            return Err(ForgeError::not_found("profile"));
        };
        let mut rec: ProfileRec = row.decode()?;
        let mut p = rec.profile.clone().unwrap_or_default();
        p.status = st.clone();
        rec.profile = Some(p);
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        Self::announce(
            tx,
            "user.status_changed",
            "user.status_set",
            &cx.actor_id,
            serde_json::json!({"set": st.is_some()}),
        )
        .await
    }

    /// Replaces the pinned items (at most six, no duplicates); the service checks that each
    /// item exists and belongs to or is visible to the caller.
    pub async fn set_pinned(
        &self,
        cx: &Cx,
        items: Vec<UserPinnedItemRef>,
    ) -> Result<Vec<UserPinnedItemRef>> {
        if items.len() > MAX_PINNED {
            return Err(ForgeError::too_large(
                "pinned_items",
                MAX_PINNED as u64,
                items.len() as u64,
            ));
        }
        let mut seen = BTreeSet::new();
        for i in &items {
            if i.kind == 0 || i.id.is_empty() || !seen.insert((i.kind, i.id.clone())) {
                return Err(ForgeError::invalid(
                    "invalid_pin",
                    "pinned items need a kind and a unique id",
                ));
            }
        }
        self.ensure(cx, &cx.actor_id).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some(mut row) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? else {
            return Err(ForgeError::not_found("profile"));
        };
        let mut rec: ProfileRec = row.decode()?;
        rec.pinned = items.clone();
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        Self::announce(
            &mut tx,
            "user.pinned_items_updated",
            "user.pin_update",
            &cx.actor_id,
            serde_json::json!({"count": items.len()}),
        )
        .await?;
        tx.commit().await?;
        Ok(items)
    }

    /// A user's pinned items.
    pub async fn pinned(&self, cx: &Cx, principal_id: &str) -> Result<Vec<UserPinnedItemRef>> {
        Ok(self.ensure(cx, principal_id).await?.0.pinned)
    }

    // ---- follows ----------------------------------------------------------------------

    /// Follows `target` (idempotent).
    pub async fn follow(&self, cx: &Cx, target: &str) -> Result<()> {
        if target == cx.actor_id {
            return Err(
                ForgeError::invalid_state("you cannot follow yourself").with_subcode("self_follow")
            );
        }
        self.ensure(cx, &cx.actor_id).await?;
        self.ensure(cx, target).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        if tx.get(&FOLLOWS, &cx.actor_id, target).await?.is_some() {
            return Ok(());
        }
        tx.insert(
            &FOLLOWS,
            &Row::new(cx.actor_id.clone(), target)
                .k1(target)
                .n1(tx.now_ms() as i64),
        )
        .await?;
        tx.add_n1(&COUNTERS, target, "followers", 1).await?;
        tx.add_n1(&COUNTERS, &cx.actor_id, "following", 1).await?;
        Self::announce(
            &mut tx,
            "follow.added",
            "follow.add",
            &cx.actor_id,
            serde_json::json!({"follower": cx.actor_id, "followee": target}),
        )
        .await?;
        tx.commit().await
    }

    /// Unfollows `target` (idempotent).
    pub async fn unfollow(&self, cx: &Cx, target: &str) -> Result<()> {
        let mut tx = self.db.begin(USERS, cx).await?;
        if !tx.delete(&FOLLOWS, &cx.actor_id, target).await? {
            return Ok(());
        }
        tx.add_n1(&COUNTERS, target, "followers", -1).await?;
        tx.add_n1(&COUNTERS, &cx.actor_id, "following", -1).await?;
        Self::announce(
            &mut tx,
            "follow.removed",
            "follow.remove",
            &cx.actor_id,
            serde_json::json!({"follower": cx.actor_id, "followee": target}),
        )
        .await?;
        tx.commit().await
    }

    /// Whether `follower` follows `target`.
    pub async fn is_following(&self, cx: &Cx, follower: &str, target: &str) -> Result<bool> {
        let mut tx = self.db.begin(USERS, cx).await?;
        Ok(tx.get(&FOLLOWS, follower, target).await?.is_some())
    }

    /// Ids of a user's followers, newest first.
    pub async fn followers(
        &self,
        cx: &Cx,
        principal_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<String>, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let scan = Scan {
            scope: None,
            key: Some(Key::K1(principal_id.to_string())),
            order: Order::N1,
            descending: true,
            limit,
            ..Scan::default()
        };
        let (rows, next) = super::common::paged(&mut tx, &FOLLOWS, scan, token, limit).await?;
        Ok((rows.into_iter().map(|r| r.scope).collect(), next))
    }

    /// Ids of the users a user follows, newest first.
    pub async fn following(
        &self,
        cx: &Cx,
        principal_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<String>, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let (rows, next) = super::common::paged(
            &mut tx,
            &FOLLOWS,
            Scan::scope(principal_id, limit).by_n1().desc(),
            token,
            limit,
        )
        .await?;
        Ok((rows.into_iter().map(|r| r.id).collect(), next))
    }

    // ---- repo follows -----------------------------------------------------------------

    /// Follows a repo as the caller (idempotent). The service has checked the caller can
    /// read it. Returns whether a follow was added.
    pub async fn follow_repo(&self, cx: &Cx, repo: &Repo) -> Result<bool> {
        self.follow_repo_as(cx, &cx.actor_id, repo).await
    }

    /// Follows a repo as `follower` (idempotent): the caller's own follow, or a repo's creator
    /// following it on creation ([`AutoFollow`]).
    pub async fn follow_repo_as(&self, cx: &Cx, follower: &str, repo: &Repo) -> Result<bool> {
        let mut tx = self.db.begin(USERS, cx).await?;
        if tx.get(&REPO_FOLLOWS, follower, &repo.id).await?.is_some() {
            return Ok(false);
        }
        tx.insert(
            &REPO_FOLLOWS,
            &Row::new(follower, repo.id.clone())
                .k1(repo.id.clone())
                .n1(tx.now_ms() as i64),
        )
        .await?;
        tx.add_n1(&COUNTERS, follower, "followed_repos", 1).await?;
        let n = tx.add_n1(&COUNTERS, &repo.id, "repo_followers", 1).await?;
        let payload =
            serde_json::json!({"repo": repo.full_name, "follower": follower, "followers": n});
        emit(
            &mut tx,
            "follow.repo_added",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Repo, &repo.id, &repo.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "follow.repo_add",
            &repo.org_id,
            &repo.id,
            SubjectKind::Repo,
            &repo.id,
            &payload,
        ))
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Stops following a repo (idempotent).
    pub async fn unfollow_repo(&self, cx: &Cx, repo: &Repo) -> Result<()> {
        self.unfollow_repo_as(cx, &cx.actor_id, repo).await
    }

    /// Stops `follower` following a repo (idempotent; an agent's feed trigger drops a source
    /// on the agent's behalf).
    pub async fn unfollow_repo_as(&self, cx: &Cx, follower: &str, repo: &Repo) -> Result<()> {
        let mut tx = self.db.begin(USERS, cx).await?;
        if !tx.delete(&REPO_FOLLOWS, follower, &repo.id).await? {
            return Ok(());
        }
        tx.add_n1(&COUNTERS, follower, "followed_repos", -1).await?;
        let n = tx.add_n1(&COUNTERS, &repo.id, "repo_followers", -1).await?;
        let payload =
            serde_json::json!({"repo": repo.full_name, "follower": follower, "followers": n});
        emit(
            &mut tx,
            "follow.repo_removed",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Repo, &repo.id, &repo.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "follow.repo_remove",
            &repo.org_id,
            &repo.id,
            SubjectKind::Repo,
            &repo.id,
            &payload,
        ))
        .await?;
        tx.commit().await
    }

    /// Whether `follower` follows a repo.
    pub async fn is_following_repo(&self, cx: &Cx, follower: &str, repo_id: &str) -> Result<bool> {
        let mut tx = self.db.begin(USERS, cx).await?;
        Ok(tx.get(&REPO_FOLLOWS, follower, repo_id).await?.is_some())
    }

    /// Repos a principal follows, newest first, as `(repo id, followed ms)`.
    pub async fn followed_repos(
        &self,
        cx: &Cx,
        principal_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<(String, u64)>, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let (rows, next) = super::common::paged(
            &mut tx,
            &REPO_FOLLOWS,
            Scan::scope(principal_id, limit).by_n1().desc(),
            token,
            limit,
        )
        .await?;
        Ok((
            rows.into_iter()
                .map(|r| (r.id, r.n1.unwrap_or(0) as u64))
                .collect(),
            next,
        ))
    }

    /// Principals who follow a repo, newest first, with the total.
    pub async fn repo_followers(
        &self,
        cx: &Cx,
        repo_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<(String, u64)>, u64, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let total = Self::counter(&mut tx, repo_id, "repo_followers").await?;
        let scan = Scan {
            scope: None,
            key: Some(Key::K1(repo_id.to_string())),
            order: Order::N1,
            descending: true,
            limit,
            ..Scan::default()
        };
        let (rows, next) = super::common::paged(&mut tx, &REPO_FOLLOWS, scan, token, limit).await?;
        Ok((
            rows.into_iter()
                .map(|r| (r.scope, r.n1.unwrap_or(0) as u64))
                .collect(),
            total,
            next,
        ))
    }

    // ---- stars ------------------------------------------------------------------------

    /// Stars a repo (idempotent). The service has checked the caller can read it.
    pub async fn star(&self, cx: &Cx, repo: &Repo) -> Result<()> {
        self.ensure(cx, &cx.actor_id).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        self.star_in(&mut tx, cx, repo).await?;
        tx.commit().await
    }

    async fn star_in(&self, tx: &mut Tx, cx: &Cx, repo: &Repo) -> Result<bool> {
        if tx.get(&STARS, &cx.actor_id, &repo.id).await?.is_some() {
            return Ok(false);
        }
        tx.insert(
            &STARS,
            &Row::new(cx.actor_id.clone(), repo.id.clone())
                .k1(repo.id.clone())
                .n1(tx.now_ms() as i64),
        )
        .await?;
        tx.add_n1(&COUNTERS, &cx.actor_id, "stars", 1).await?;
        let n = tx.add_n1(&COUNTERS, &repo.id, "stargazers", 1).await?;
        let payload = serde_json::json!({"action": "started", "repo": repo.full_name, "stargazers": n, "user": cx.actor_id});
        emit(
            tx,
            "star.added",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Repo, &repo.id, &repo.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "star.add",
            &repo.org_id,
            &repo.id,
            SubjectKind::Repo,
            &repo.id,
            &payload,
        ))
        .await?;
        Ok(true)
    }

    /// Unstars a repo (idempotent); the repo also leaves the caller's star lists.
    pub async fn unstar(&self, cx: &Cx, repo: &Repo) -> Result<()> {
        let mut tx = self.db.begin(USERS, cx).await?;
        if !tx.delete(&STARS, &cx.actor_id, &repo.id).await? {
            return Ok(());
        }
        tx.add_n1(&COUNTERS, &cx.actor_id, "stars", -1).await?;
        let n = tx.add_n1(&COUNTERS, &repo.id, "stargazers", -1).await?;
        for l in tx
            .scan(&STAR_LISTS, &Scan::scope(cx.actor_id.clone(), 1000))
            .await?
        {
            tx.delete(&STAR_LIST_ITEMS, &l.id, &repo.id).await?;
        }
        let payload = serde_json::json!({"action": "stopped", "repo": repo.full_name, "stargazers": n, "user": cx.actor_id});
        emit(
            &mut tx,
            "star.removed",
            &repo.org_id,
            &repo.id,
            subject(SubjectKind::Repo, &repo.id, &repo.id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(
            "star.remove",
            &repo.org_id,
            &repo.id,
            SubjectKind::Repo,
            &repo.id,
            &payload,
        ))
        .await?;
        tx.commit().await
    }

    /// Whether the caller starred a repo, and which of their lists hold it.
    pub async fn is_starred(&self, cx: &Cx, repo_id: &str) -> Result<(bool, Vec<String>)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let starred = tx.get(&STARS, &cx.actor_id, repo_id).await?.is_some();
        let mut lists = Vec::new();
        for l in tx
            .scan(&STAR_LISTS, &Scan::scope(cx.actor_id.clone(), 1000))
            .await?
        {
            if tx.get(&STAR_LIST_ITEMS, &l.id, repo_id).await?.is_some() {
                lists.push(l.id);
            }
        }
        Ok((starred, lists))
    }

    /// Repos a user starred, newest first, as `(repo id, starred ms)`.
    pub async fn starred(
        &self,
        cx: &Cx,
        principal_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<(String, u64)>, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let (rows, next) = super::common::paged(
            &mut tx,
            &STARS,
            Scan::scope(principal_id, limit).by_n1().desc(),
            token,
            limit,
        )
        .await?;
        Ok((
            rows.into_iter()
                .map(|r| (r.id, r.n1.unwrap_or(0) as u64))
                .collect(),
            next,
        ))
    }

    /// Principals who starred a repo, newest first, with the total.
    pub async fn stargazers(
        &self,
        cx: &Cx,
        repo_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<(String, u64)>, u64, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let total = Self::counter(&mut tx, repo_id, "stargazers").await?;
        let scan = Scan {
            scope: None,
            key: Some(Key::K1(repo_id.to_string())),
            order: Order::N1,
            descending: true,
            limit,
            ..Scan::default()
        };
        let (rows, next) = super::common::paged(&mut tx, &STARS, scan, token, limit).await?;
        Ok((
            rows.into_iter()
                .map(|r| (r.scope, r.n1.unwrap_or(0) as u64))
                .collect(),
            total,
            next,
        ))
    }

    /// Stars of a repo.
    pub async fn star_count(&self, cx: &Cx, repo_id: &str) -> Result<u64> {
        let mut tx = self.db.begin(USERS, cx).await?;
        Self::counter(&mut tx, repo_id, "stargazers").await
    }

    // ---- star lists -------------------------------------------------------------------

    async fn list_view(tx: &mut Tx, rec: StarList, row: &Row) -> Result<StarList> {
        let mut l = rec;
        l.repo_count = tx.count(&STAR_LIST_ITEMS, &row.id, None).await? as u32;
        Ok(l)
    }

    /// Creates a star list.
    pub async fn create_star_list(
        &self,
        cx: &Cx,
        name: &str,
        description: &str,
        private: bool,
    ) -> Result<StarList> {
        required("name", name)?;
        max_len("name", name, 32)?;
        max_len("description", description, 160)?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let existing = tx
            .scan_msgs::<StarList>(
                &STAR_LISTS,
                &Scan::scope(cx.actor_id.clone(), MAX_STAR_LISTS + 1),
            )
            .await?;
        if existing.len() >= MAX_STAR_LISTS {
            return Err(ForgeError::too_large(
                "star_lists",
                MAX_STAR_LISTS as u64,
                existing.len() as u64 + 1,
            ));
        }
        if existing
            .iter()
            .any(|(l, _)| l.name.eq_ignore_ascii_case(name.trim()))
        {
            return Err(ForgeError::already_exists(format!("star list {name}")));
        }
        let now = tx.now_ms();
        let l = StarList {
            id: tx.new_id()?,
            owner: Some(cx.principal()),
            name: name.trim().to_string(),
            description: description.to_string(),
            private,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        tx.insert(
            &STAR_LISTS,
            &Row::new(cx.actor_id.clone(), l.id.clone())
                .k1(name.trim().to_lowercase())
                .n1(now as i64)
                .msg(&l),
        )
        .await?;
        emit(
            &mut tx,
            "star_list.changed",
            "",
            "",
            subject(SubjectKind::User, "", &cx.actor_id),
            serde_json::json!({"action": "created", "list": &l}),
        )
        .await?;
        tx.audit(draft(
            "star_list.create",
            "",
            "",
            SubjectKind::User,
            &l.id,
            &serde_json::json!({"name": l.name, "private": private}),
        ))
        .await?;
        tx.commit().await?;
        Ok(l)
    }

    async fn load_list(tx: &mut Tx, owner: &str, id: &str) -> Result<(StarList, Row)> {
        tx.get_msg::<StarList>(&STAR_LISTS, owner, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("star list {id}")))
    }

    async fn find_list(tx: &mut Tx, viewer: &str, id: &str) -> Result<(StarList, Row)> {
        // Lists are scoped by owner; look the owner up through the list rows themselves.
        let rows = tx
            .scan(
                &STAR_LISTS,
                &Scan {
                    scope: None,
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        for r in rows {
            if r.id == id {
                let l: StarList = r.decode()?;
                let owner = l.owner.as_ref().map(|o| o.id.as_str()).unwrap_or("");
                if l.private && owner != viewer {
                    break;
                }
                return Ok((l, r));
            }
        }
        Err(ForgeError::not_found(format!("star list {id}")))
    }

    /// A star list; private lists are visible to their owner only.
    pub async fn star_list(&self, cx: &Cx, id: &str) -> Result<StarList> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let (l, row) = Self::find_list(&mut tx, &cx.actor_id, id).await?;
        Self::list_view(&mut tx, l, &row).await
    }

    /// A user's star lists (their private lists only for themselves).
    pub async fn star_lists(
        &self,
        cx: &Cx,
        owner: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<StarList>, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan_msgs::<StarList>(
                &STAR_LISTS,
                &Scan::scope(owner, MAX_STAR_LISTS * 2).by_n1().desc(),
            )
            .await?;
        let mut visible = Vec::new();
        for (l, row) in rows {
            if l.private && owner != cx.actor_id {
                continue;
            }
            visible.push(Self::list_view(&mut tx, l, &row).await?);
        }
        page_vec(visible, token, limit)
    }

    /// Edits a star list (owner only).
    pub async fn update_star_list(
        &self,
        cx: &Cx,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        private: Option<bool>,
    ) -> Result<StarList> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let (mut l, mut row) = Self::load_list(&mut tx, &cx.actor_id, id).await?;
        if let Some(n) = name {
            required("name", n)?;
            max_len("name", n, 32)?;
            let others = tx
                .scan_msgs::<StarList>(&STAR_LISTS, &Scan::scope(cx.actor_id.clone(), 1000))
                .await?;
            if others
                .iter()
                .any(|(o, _)| o.id != id && o.name.eq_ignore_ascii_case(n.trim()))
            {
                return Err(ForgeError::already_exists(format!("star list {n}")));
            }
            l.name = n.trim().to_string();
            row.k1 = Some(n.trim().to_lowercase());
        }
        if let Some(d) = description {
            max_len("description", d, 160)?;
            l.description = d.to_string();
        }
        if let Some(p) = private {
            l.private = p;
        }
        let now = tx.now_ms();
        l.updated_at = Some(ts(now));
        row.n1 = Some(now as i64);
        row.body = l.encode_to_vec();
        tx.update(&STAR_LISTS, &row).await?;
        emit(
            &mut tx,
            "star_list.changed",
            "",
            "",
            subject(SubjectKind::User, "", &cx.actor_id),
            serde_json::json!({"action": "updated", "list": &l}),
        )
        .await?;
        tx.audit(draft(
            "star_list.update",
            "",
            "",
            SubjectKind::User,
            id,
            &serde_json::json!({"name": l.name, "private": l.private}),
        ))
        .await?;
        let out = Self::list_view(&mut tx, l, &row).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Deletes a star list (the stars stay).
    pub async fn delete_star_list(&self, cx: &Cx, id: &str) -> Result<()> {
        let mut tx = self.db.begin(USERS, cx).await?;
        Self::load_list(&mut tx, &cx.actor_id, id).await?;
        for r in tx
            .scan(&STAR_LIST_ITEMS, &Scan::scope(id, crate::store::MAX_SCAN))
            .await?
        {
            tx.delete(&STAR_LIST_ITEMS, id, &r.id).await?;
        }
        tx.delete(&STAR_LISTS, &cx.actor_id, id).await?;
        emit(
            &mut tx,
            "star_list.changed",
            "",
            "",
            subject(SubjectKind::User, "", &cx.actor_id),
            serde_json::json!({"action": "deleted", "list_id": id}),
        )
        .await?;
        tx.audit(draft(
            "star_list.delete",
            "",
            "",
            SubjectKind::User,
            id,
            &serde_json::Value::Null,
        ))
        .await?;
        tx.commit().await
    }

    /// Adds a repo to a list, starring it first when needed.
    pub async fn add_to_star_list(&self, cx: &Cx, id: &str, repo: &Repo) -> Result<()> {
        let mut tx = self.db.begin(USERS, cx).await?;
        Self::load_list(&mut tx, &cx.actor_id, id).await?;
        self.star_in(&mut tx, cx, repo).await?;
        if tx.get(&STAR_LIST_ITEMS, id, &repo.id).await?.is_none() {
            tx.insert(
                &STAR_LIST_ITEMS,
                &Row::new(id, repo.id.clone()).n1(tx.now_ms() as i64),
            )
            .await?;
            emit(
                &mut tx,
                "star_list.changed",
                "",
                "",
                subject(SubjectKind::User, "", &cx.actor_id),
                serde_json::json!({"action": "repo_added", "list_id": id, "repo": repo.full_name}),
            )
            .await?;
            tx.audit(draft(
                "star_list.add_repo",
                "",
                &repo.id,
                SubjectKind::User,
                id,
                &serde_json::json!({"repo": repo.full_name}),
            ))
            .await?;
        }
        tx.commit().await
    }

    /// Removes a repo from a list (the star stays).
    pub async fn remove_from_star_list(&self, cx: &Cx, id: &str, repo_id: &str) -> Result<()> {
        let mut tx = self.db.begin(USERS, cx).await?;
        Self::load_list(&mut tx, &cx.actor_id, id).await?;
        if tx.delete(&STAR_LIST_ITEMS, id, repo_id).await? {
            emit(
                &mut tx,
                "star_list.changed",
                "",
                "",
                subject(SubjectKind::User, "", &cx.actor_id),
                serde_json::json!({"action": "repo_removed", "list_id": id, "repo_id": repo_id}),
            )
            .await?;
            tx.audit(draft(
                "star_list.remove_repo",
                "",
                repo_id,
                SubjectKind::User,
                id,
                &serde_json::Value::Null,
            ))
            .await?;
        }
        tx.commit().await
    }

    /// Repos in a list, newest first, as `(repo id, added ms)`.
    pub async fn star_list_repos(
        &self,
        cx: &Cx,
        id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<(String, u64)>, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        Self::find_list(&mut tx, &cx.actor_id, id).await?;
        let (rows, next) = super::common::paged(
            &mut tx,
            &STAR_LIST_ITEMS,
            Scan::scope(id, limit).by_n1().desc(),
            token,
            limit,
        )
        .await?;
        Ok((
            rows.into_iter()
                .map(|r| (r.id, r.n1.unwrap_or(0) as u64))
                .collect(),
            next,
        ))
    }

    // ---- emails -----------------------------------------------------------------------

    /// The generated noreply address of an account.
    pub fn noreply_address(&self, principal_id: &str, login: &str) -> String {
        format!("{principal_id}+{login}@{}", self.cfg.noreply_domain)
    }

    async fn verified_owner(tx: &mut Tx, email: &str) -> Result<Option<String>> {
        let rows = tx
            .scan(
                &EMAILS,
                &Scan {
                    scope: None,
                    key: Some(Key::K1(email.to_string())),
                    limit: 20,
                    ..Scan::default()
                },
            )
            .await?;
        Ok(rows
            .into_iter()
            .find(|r| r.k2.as_deref() == Some("v"))
            .map(|r| r.scope))
    }

    /// Adds an unverified email and queues its verification token.
    pub async fn add_email(&self, cx: &Cx, email: &str) -> Result<UserEmail> {
        let email = validate_email(email)?;
        let (rec, _) = self.ensure(cx, &cx.actor_id).await?;
        let login = rec.profile.map(|p| p.login).unwrap_or_default();
        if email == self.noreply_address(&cx.actor_id, &login).to_lowercase()
            || email.ends_with(&format!("@{}", self.cfg.noreply_domain))
        {
            return Err(ForgeError::invalid(
                "noreply_address",
                "noreply addresses are generated; they cannot be added",
            ));
        }
        let mut tx = self.db.begin(USERS, cx).await?;
        if tx.get(&EMAILS, &cx.actor_id, &email).await?.is_some() {
            return Err(ForgeError::already_exists(format!("email {email}")));
        }
        if let Some(owner) = Self::verified_owner(&mut tx, &email).await?
            && owner != cx.actor_id
        {
            return Err(
                ForgeError::already_exists(format!("email {email}")).with_subcode("email_taken")
            );
        }
        let mine = tx.count(&EMAILS, &cx.actor_id, None).await?;
        if mine as usize >= MAX_EMAILS {
            return Err(ForgeError::too_large("emails", MAX_EMAILS as u64, mine + 1));
        }
        let now = tx.now_ms();
        let e = UserEmail {
            email: email.clone(),
            added_at: Some(ts(now)),
            ..Default::default()
        };
        tx.insert(
            &EMAILS,
            &Row::new(cx.actor_id.clone(), email.clone())
                .k1(email.clone())
                .k2("u")
                .n1(now as i64)
                .msg(&EmailRec {
                    email: Some(e.clone()),
                }),
        )
        .await?;
        self.queue_verification(&mut tx, &cx.actor_id, &email, now)
            .await?;
        Self::security(
            &mut tx,
            &cx.actor_id,
            cx,
            "email.add",
            serde_json::json!({"email": email}),
        )
        .await?;
        Self::announce(
            &mut tx,
            "user.email_changed",
            "user.email_add",
            &cx.actor_id,
            serde_json::json!({"action": "added", "email": email}),
        )
        .await?;
        tx.commit().await?;
        Ok(e)
    }

    async fn queue_verification(
        &self,
        tx: &mut Tx,
        principal: &str,
        email: &str,
        now: u64,
    ) -> Result<String> {
        let token = self.db.ids().random_hex(16)?;
        let rec = VerifyRec {
            principal_id: principal.to_string(),
            email: email.to_string(),
            expires_ms: now + 3 * DAY_MS,
            sent: false,
            token: token.clone(),
        };
        tx.insert(
            &EMAIL_TOKENS,
            &Row::new("", token.clone()).n1(now as i64).msg(&rec),
        )
        .await?;
        Ok(token)
    }

    /// Verification tokens the mailer has not sent yet (marked sent as returned).
    pub async fn claim_verifications(&self, cx: &Cx, limit: usize) -> Result<Vec<VerifyRec>> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan(
                &EMAIL_TOKENS,
                &Scan::scope("", crate::store::MAX_SCAN).by_n1(),
            )
            .await?;
        let now = tx.now_ms();
        let mut out = Vec::new();
        for mut row in rows {
            let mut rec: VerifyRec = row.decode()?;
            if rec.sent || rec.expires_ms <= now {
                continue;
            }
            rec.sent = true;
            row.body = rec.encode_to_vec();
            tx.update(&EMAIL_TOKENS, &row).await?;
            out.push(rec);
            if out.len() >= limit {
                break;
            }
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Issues a fresh token for an unverified address (the mailer sends it).
    pub async fn resend_verification(&self, cx: &Cx, email: &str) -> Result<u64> {
        let email = validate_email(email)?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some((rec, _)) = tx
            .get_msg::<EmailRec>(&EMAILS, &cx.actor_id, &email)
            .await?
        else {
            return Err(ForgeError::not_found(format!("email {email}")));
        };
        if rec.email.is_some_and(|e| e.verified) {
            return Err(ForgeError::invalid_state("the address is already verified"));
        }
        let now = tx.now_ms();
        self.queue_verification(&mut tx, &cx.actor_id, &email, now)
            .await?;
        tx.commit().await?;
        Ok(now)
    }

    /// Redeems a verification token: the address becomes verified, claims any placeholder
    /// identity registered for it, and becomes primary if the account has none.
    pub async fn verify_email(&self, token: &str) -> Result<UserEmail> {
        let cx = Cx::system();
        let mut tx = self.db.begin(USERS, &cx).await?;
        let Some((v, _)) = tx.get_msg::<VerifyRec>(&EMAIL_TOKENS, "", token).await? else {
            return Err(ForgeError::not_found("verification token"));
        };
        let now = tx.now_ms();
        if v.expires_ms <= now {
            return Err(ForgeError::invalid_state("the verification token expired")
                .with_subcode("token_expired"));
        }
        tx.add_n1(&COUNTERS, "", &format!("verified_email:{}", v.email), 0)
            .await?;
        if let Some(owner) = Self::verified_owner(&mut tx, &v.email).await?
            && owner != v.principal_id
        {
            return Err(ForgeError::already_exists(format!("email {}", v.email))
                .with_subcode("email_taken"));
        }
        let Some(mut row) = tx
            .get_for_update(&EMAILS, &v.principal_id, &v.email)
            .await?
        else {
            return Err(ForgeError::not_found(format!("email {}", v.email)));
        };
        let mut rec: EmailRec = row.decode()?;
        let mut e = rec.email.clone().unwrap_or_default();
        e.verified = true;
        e.verified_at = Some(ts(now));
        let has_primary = tx
            .scan_msgs::<EmailRec>(&EMAILS, &Scan::scope(v.principal_id.clone(), 100))
            .await?
            .iter()
            .any(|(r, _)| r.email.as_ref().is_some_and(|x| x.primary));
        e.primary = !has_primary;
        rec.email = Some(e.clone());
        row.k2 = Some("v".into());
        row.body = rec.encode_to_vec();
        tx.update(&EMAILS, &row).await?;
        tx.delete(&EMAIL_TOKENS, "", token).await?;
        let claimed = tx.delete(&PLACEHOLDERS, "", &v.email).await?;
        let ctx = Cx::new(v.principal_id.clone(), PrincipalKind::Human);
        Self::security(
            &mut tx,
            &v.principal_id,
            &ctx,
            "email.verify",
            serde_json::json!({"email": v.email, "claimed_placeholder": claimed}),
        )
        .await?;
        Self::announce(&mut tx, "user.email_changed", "user.email_verify", &v.principal_id, serde_json::json!({"action": "verified", "email": v.email, "claimed_placeholder": claimed})).await?;
        tx.commit().await?;
        Ok(e)
    }

    /// The caller's emails (verified ones first), with the noreply address and privacy flags.
    pub async fn emails(&self, cx: &Cx) -> Result<(Vec<UserEmail>, String, bool, bool)> {
        let (rec, _) = self.ensure(cx, &cx.actor_id).await?;
        let login = rec.profile.clone().map(|p| p.login).unwrap_or_default();
        let public = rec.profile.map(|p| p.public_email).unwrap_or_default();
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan_msgs::<EmailRec>(&EMAILS, &Scan::scope(cx.actor_id.clone(), 100).by_n1())
            .await?;
        let mut out: Vec<UserEmail> = rows
            .into_iter()
            .filter_map(|(r, _)| r.email)
            .map(|mut e| {
                e.public = e.email == public;
                e
            })
            .collect();
        out.sort_by_key(|e| (!e.verified, !e.primary));
        Ok((
            out,
            self.noreply_address(&cx.actor_id, &login),
            rec.keep_private,
            rec.block_private_pushes,
        ))
    }

    /// Removes an email; the primary one only while another verified address can replace it.
    pub async fn delete_email(&self, cx: &Cx, email: &str) -> Result<()> {
        let email = validate_email(email)?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some((rec, _)) = tx
            .get_msg::<EmailRec>(&EMAILS, &cx.actor_id, &email)
            .await?
        else {
            return Err(ForgeError::not_found(format!("email {email}")));
        };
        if rec.email.is_some_and(|e| e.primary) {
            return Err(ForgeError::invalid_state(
                "the primary email cannot be removed; make another address primary first",
            )
            .with_subcode("primary_email"));
        }
        tx.delete(&EMAILS, &cx.actor_id, &email).await?;
        if let Some(mut prow) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? {
            let mut prec: ProfileRec = prow.decode()?;
            if let Some(p) = prec.profile.as_mut()
                && p.public_email == email
            {
                p.public_email.clear();
                prow.body = prec.encode_to_vec();
                tx.update(&PROFILES, &prow).await?;
            }
        }
        Self::security(
            &mut tx,
            &cx.actor_id,
            cx,
            "email.delete",
            serde_json::json!({"email": email}),
        )
        .await?;
        Self::announce(
            &mut tx,
            "user.email_changed",
            "user.email_delete",
            &cx.actor_id,
            serde_json::json!({"action": "removed", "email": email}),
        )
        .await?;
        tx.commit().await
    }

    /// Makes a verified address primary.
    pub async fn set_primary_email(&self, cx: &Cx, email: &str) -> Result<()> {
        let email = validate_email(email)?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan(&EMAILS, &Scan::scope(cx.actor_id.clone(), 100))
            .await?;
        let target = rows
            .iter()
            .find(|r| r.id == email)
            .ok_or_else(|| ForgeError::not_found(format!("email {email}")))?;
        if !target
            .decode::<EmailRec>()?
            .email
            .is_some_and(|e| e.verified)
        {
            return Err(
                ForgeError::invalid_state("only a verified address can be primary")
                    .with_subcode("unverified_email"),
            );
        }
        for mut row in rows {
            let mut rec: EmailRec = row.decode()?;
            let Some(mut e) = rec.email.clone() else {
                continue;
            };
            let want = row.id == email;
            if e.primary != want {
                e.primary = want;
                rec.email = Some(e);
                row.body = rec.encode_to_vec();
                tx.update(&EMAILS, &row).await?;
            }
        }
        Self::security(
            &mut tx,
            &cx.actor_id,
            cx,
            "email.primary",
            serde_json::json!({"email": email}),
        )
        .await?;
        Self::announce(
            &mut tx,
            "user.email_changed",
            "user.email_primary",
            &cx.actor_id,
            serde_json::json!({"action": "primary", "email": email}),
        )
        .await?;
        tx.commit().await
    }

    /// Email privacy: use the noreply address for web operations, block pushes authored with
    /// a private email, and choose which verified addresses are public.
    pub async fn update_email_visibility(
        &self,
        cx: &Cx,
        keep_private: Option<bool>,
        block_pushes: Option<bool>,
        public: &[String],
        replace_public: bool,
    ) -> Result<()> {
        self.ensure(cx, &cx.actor_id).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some(mut row) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? else {
            return Err(ForgeError::not_found("profile"));
        };
        let mut rec: ProfileRec = row.decode()?;
        if let Some(k) = keep_private {
            rec.keep_private = k;
        }
        if let Some(b) = block_pushes {
            rec.block_private_pushes = b;
        }
        if replace_public {
            if public.len() > 1 {
                return Err(ForgeError::invalid(
                    "one_public_email",
                    "one email can be public at a time",
                ));
            }
            let mut p = rec.profile.clone().unwrap_or_default();
            p.public_email = match public.first() {
                None => String::new(),
                Some(e) => {
                    let e = validate_email(e)?;
                    match tx.get_msg::<EmailRec>(&EMAILS, &cx.actor_id, &e).await? {
                        Some((r, _)) if r.email.as_ref().is_some_and(|x| x.verified) => e,
                        _ => {
                            return Err(ForgeError::invalid(
                                "unverified_email",
                                "only verified addresses can be public",
                            ));
                        }
                    }
                }
            };
            rec.profile = Some(p);
        }
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        Self::security(&mut tx, &cx.actor_id, cx, "email.visibility", serde_json::json!({"keep_private": rec.keep_private, "block_private_pushes": rec.block_private_pushes})).await?;
        Self::announce(
            &mut tx,
            "user.email_changed",
            "user.email_visibility",
            &cx.actor_id,
            serde_json::json!({"action": "visibility", "keep_private": rec.keep_private}),
        )
        .await?;
        tx.commit().await
    }

    /// Registers the placeholder identity an import created for a commit email.
    pub async fn register_placeholder(
        &self,
        cx: &Cx,
        email: &str,
        placeholder: &Principal,
    ) -> Result<()> {
        let email = validate_email(email)?;
        let mut tx = self.db.begin(USERS, cx).await?;
        if Self::verified_owner(&mut tx, &email).await?.is_some() {
            return Err(
                ForgeError::already_exists(format!("email {email}")).with_subcode("email_taken")
            );
        }
        tx.put(
            &PLACEHOLDERS,
            &Row::new("", email.clone())
                .k1(placeholder.id.clone())
                .msg(placeholder),
        )
        .await?;
        tx.audit(draft(
            "user.placeholder_register",
            "",
            "",
            SubjectKind::User,
            &placeholder.id,
            &serde_json::json!({"email": email}),
        ))
        .await?;
        tx.commit().await
    }

    /// Maps commit emails to principals (same order as the input): noreply addresses and
    /// verified emails to their account, placeholders to the imported identity.
    pub async fn resolve_commit_emails(
        &self,
        cx: &Cx,
        emails: &[String],
    ) -> Result<Vec<UserCommitEmailMapping>> {
        if emails.len() > 100 {
            return Err(ForgeError::too_large("emails", 100, emails.len() as u64));
        }
        let mut out = Vec::new();
        for raw in emails {
            let e = raw.trim().to_lowercase();
            let mut mapping = UserCommitEmailMapping {
                email: raw.clone(),
                ..Default::default()
            };
            let mut pid: Option<String> = None;
            if let Some(local) = e.strip_suffix(&format!("@{}", self.cfg.noreply_domain)) {
                let (head, login) = match local.split_once('+') {
                    Some((id, login)) => (Some(id.to_string()), login.to_string()),
                    None => (None, local.to_string()),
                };
                pid = match head {
                    Some(id) => {
                        // Addresses are lower-cased; ULIDs are upper case.
                        let mut found = None;
                        for cand in [id.clone(), id.to_uppercase()] {
                            if let Ok((rec, _)) = self.ensure(cx, &cand).await
                                && rec.deleted_ms == 0
                                && rec
                                    .profile
                                    .is_some_and(|p| p.login.eq_ignore_ascii_case(&login))
                            {
                                found = Some(cand);
                                break;
                            }
                        }
                        found
                    }
                    None => self.resolve_login(cx, &login).await.ok().map(|(p, _, _)| p),
                };
            } else {
                let mut tx = self.db.begin(USERS, cx).await?;
                if let Some(owner) = Self::verified_owner(&mut tx, &e).await? {
                    pid = Some(owner);
                } else if let Some((p, _)) = tx.get_msg::<Principal>(&PLACEHOLDERS, "", &e).await? {
                    mapping.principal = Some(p);
                    mapping.placeholder = true;
                }
            }
            if let Some(id) = pid {
                mapping.principal = Some(self.directory.get(&id).await.unwrap_or(Principal {
                    id,
                    ..Default::default()
                }));
            }
            out.push(mapping);
        }
        Ok(out)
    }

    /// Whether a write authored with `email` is allowed for `principal_id`: it is unless the
    /// account blocks pushes with private emails and the address is a verified, non-public one.
    pub async fn commit_email_allowed(
        &self,
        cx: &Cx,
        principal_id: &str,
        email: &str,
    ) -> Result<bool> {
        let (rec, _) = self.ensure(cx, principal_id).await?;
        if !rec.block_private_pushes {
            return Ok(true);
        }
        let e = email.trim().to_lowercase();
        let public = rec.profile.map(|p| p.public_email).unwrap_or_default();
        if e.ends_with(&format!("@{}", self.cfg.noreply_domain)) || e == public {
            return Ok(true);
        }
        let mut tx = self.db.begin(USERS, cx).await?;
        Ok(tx.get(&EMAILS, principal_id, &e).await?.is_none())
    }

    // ---- settings ---------------------------------------------------------------------

    /// The caller's settings.
    pub async fn settings(&self, cx: &Cx) -> Result<UserSettings> {
        Ok(self
            .ensure(cx, &cx.actor_id)
            .await?
            .0
            .settings
            .unwrap_or_else(default_user_settings))
    }

    /// Applies a settings patch with validation.
    pub async fn update_settings(&self, cx: &Cx, p: SettingsPatch) -> Result<UserSettings> {
        self.ensure(cx, &cx.actor_id).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some(mut row) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? else {
            return Err(ForgeError::not_found("profile"));
        };
        let mut rec: ProfileRec = row.decode()?;
        let mut s = rec.settings.clone().unwrap_or_else(default_user_settings);
        for (f, v, slot) in [
            ("theme", &p.theme, &mut s.theme),
            ("dark_theme", &p.dark_theme, &mut s.dark_theme),
            ("light_theme", &p.light_theme, &mut s.light_theme),
        ] {
            if let Some(v) = v {
                max_len(f, v, 50)?;
                if f == "theme" {
                    required(f, v)?;
                }
                *slot = v.clone();
            }
        }
        if let Some(z) = &p.time_zone {
            Zone::parse(z)?;
            s.time_zone = z.clone();
        }
        if let Some(l) = &p.locale {
            let ok = !l.is_empty()
                && l.split('-').enumerate().all(|(i, part)| {
                    !part.is_empty()
                        && part.len() <= 8
                        && part.bytes().all(|b| b.is_ascii_alphanumeric())
                        && (i > 0 || part.bytes().all(|b| b.is_ascii_alphabetic()))
                });
            if !ok {
                return Err(ForgeError::invalid(
                    "invalid_locale",
                    format!("{l:?} is not a BCP 47 locale"),
                ));
            }
            s.locale = l.clone();
        }
        if let Some(i) = &p.editor_indent {
            let ok = i.split_once(':').is_some_and(|(k, n)| {
                (k == "spaces" || k == "tabs")
                    && n.parse::<u8>().is_ok_and(|n| (1..=8).contains(&n))
            });
            if !ok {
                return Err(ForgeError::invalid(
                    "invalid_indent",
                    "editor_indent is `spaces:N` or `tabs:N` with N from 1 to 8",
                ));
            }
            s.editor_indent = i.clone();
        }
        if let Some(d) = &p.diff_view {
            if d != "unified" && d != "split" {
                return Err(ForgeError::invalid(
                    "invalid_diff_view",
                    "diff_view is `unified` or `split`",
                ));
            }
            s.diff_view = d.clone();
        }
        if let Some(v) = p.keyboard_shortcuts_enabled {
            s.keyboard_shortcuts_enabled = v;
        }
        if let Some(v) = p.show_private_contributions {
            s.show_private_contributions = v;
        }
        if let Some(v) = p.vigilant_mode {
            s.vigilant_mode = v;
        }
        for (target, patch) in [
            (&mut s.accessibility, &p.accessibility),
            (&mut s.keyboard_shortcuts, &p.keyboard_shortcuts),
        ] {
            for kv in patch {
                required("key", &kv.key)?;
                max_len("key", &kv.key, 64)?;
                max_len("value", &kv.value, 64)?;
                target.retain(|x| x.key != kv.key);
                if !kv.value.is_empty() {
                    target.push(kv.clone());
                }
            }
            target.sort_by(|a, b| a.key.cmp(&b.key));
            if target.len() > 100 {
                return Err(ForgeError::too_large(
                    "settings_entries",
                    100,
                    target.len() as u64,
                ));
            }
        }
        rec.settings = Some(s.clone());
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        Self::announce(
            &mut tx,
            "user.settings_updated",
            "user.settings_update",
            &cx.actor_id,
            serde_json::json!({"settings": &s}),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    // ---- security log -----------------------------------------------------------------

    /// The caller's security log, newest first.
    pub async fn security_events(
        &self,
        cx: &Cx,
        prefix: &str,
        since_ms: u64,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<UserSecurityEvent>, String)> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan_msgs::<UserSecurityEvent>(
                &SECURITY_EVENTS,
                &Scan::scope(cx.actor_id.clone(), crate::store::MAX_SCAN)
                    .by_n1()
                    .desc()
                    .n1_min(since_ms as i64),
            )
            .await?;
        let v: Vec<UserSecurityEvent> = rows
            .into_iter()
            .map(|(e, _)| e)
            .filter(|e| e.action.starts_with(prefix))
            .collect();
        page_vec(v, token, limit)
    }

    // ---- login rename and account lifecycle -------------------------------------------

    /// Renames the caller's login; the old name redirects for [`REDIRECT_DAYS`]. The service
    /// has checked that no org or other directory entry holds the new name.
    pub async fn rename_login(&self, cx: &Cx, new_login: &str) -> Result<UserProfile> {
        validate_login(new_login)?;
        let (rec0, _) = self.ensure(cx, &cx.actor_id).await?;
        let old = rec0.profile.map(|p| p.login).unwrap_or_default();
        let mut tx = self.db.begin(USERS, cx).await?;
        let now = tx.now_ms();
        let new_key = new_login.to_lowercase();
        if new_key == old.to_lowercase() && new_login == old {
            return Err(ForgeError::invalid(
                "same_login",
                "that is already your login",
            ));
        }
        if let Some(r) = tx.get(&LOGINS, "", &new_key).await? {
            let mine = r.k1.as_deref() == Some(cx.actor_id.as_str());
            let expired = r.n1.is_some_and(|n| n as u64 <= now);
            if !(mine || expired) {
                return Err(ForgeError::already_exists(format!("login {new_login}")));
            }
        }
        let Some(mut row) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? else {
            return Err(ForgeError::not_found("profile"));
        };
        let mut rec: ProfileRec = row.decode()?;
        let mut p = rec.profile.clone().unwrap_or_default();
        p.login = new_login.to_string();
        p.updated_at = Some(ts(now));
        rec.profile = Some(p);
        row.k1 = Some(new_key.clone());
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        if new_key != old.to_lowercase() {
            tx.put(
                &LOGINS,
                &Row::new("", old.to_lowercase())
                    .k1(cx.actor_id.clone())
                    .n1((now + REDIRECT_DAYS * DAY_MS) as i64),
            )
            .await?;
        }
        tx.put(&LOGINS, &Row::new("", new_key).k1(cx.actor_id.clone()))
            .await?;
        Self::security(
            &mut tx,
            &cx.actor_id,
            cx,
            "login.rename",
            serde_json::json!({"from": old, "to": new_login}),
        )
        .await?;
        Self::announce(
            &mut tx,
            "user.login_renamed",
            "user.login_rename",
            &cx.actor_id,
            serde_json::json!({"from": old, "to": new_login}),
        )
        .await?;
        tx.commit().await?;
        self.profile(cx, &cx.actor_id, &cx.actor_id).await
    }

    /// Requests a data export (one pending request at a time).
    pub async fn request_export(
        &self,
        cx: &Cx,
        include_repos: bool,
        include_attachments: bool,
    ) -> Result<UserDataExport> {
        self.ensure(cx, &cx.actor_id).await?;
        let mut tx = self.db.begin(USERS, cx).await?;
        let now = tx.now_ms();
        let rows = tx
            .scan_msgs::<ExportRec>(&EXPORTS, &Scan::scope(cx.actor_id.clone(), 100))
            .await?;
        if rows.iter().any(|(r, _)| {
            r.export.as_ref().is_some_and(|e| {
                e.state == UserDataExportState::Pending as i32
                    || e.state == UserDataExportState::Running as i32
            })
        }) {
            return Err(
                ForgeError::already_exists("a pending data export").with_subcode("export_pending")
            );
        }
        let id = tx.new_id()?;
        let e = UserDataExport {
            id: id.clone(),
            state: UserDataExportState::Pending as i32,
            created_at: Some(ts(now)),
            ..Default::default()
        };
        tx.insert(
            &EXPORTS,
            &Row::new(cx.actor_id.clone(), id.clone())
                .n1(now as i64)
                .msg(&ExportRec {
                    export: Some(e.clone()),
                    include_repos,
                    include_attachments,
                    key: String::new(),
                }),
        )
        .await?;
        Self::security(
            &mut tx,
            &cx.actor_id,
            cx,
            "export.request",
            serde_json::json!({"export_id": id}),
        )
        .await?;
        Self::announce(&mut tx, "user.export_requested", "user.export_request", &cx.actor_id, serde_json::json!({"export_id": id, "include_repos": include_repos, "include_attachments": include_attachments})).await?;
        tx.commit().await?;
        Ok(e)
    }

    /// An export as the owner sees it: READY exports carry a signed URL; after a week they
    /// are `EXPIRED`.
    pub async fn export(&self, cx: &Cx, id: &str) -> Result<UserDataExport> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some((rec, _)) = tx.get_msg::<ExportRec>(&EXPORTS, &cx.actor_id, id).await? else {
            return Err(ForgeError::not_found(format!("data export {id}")));
        };
        let now = tx.now_ms();
        let mut e = rec.export.unwrap_or_default();
        if e.state == UserDataExportState::Ready as i32 {
            let done = e.completed_at.as_ref().map(ts_ms).unwrap_or(0);
            if done + 7 * DAY_MS <= now {
                e.state = UserDataExportState::Expired as i32;
            } else {
                let s = self.signer.download(&rec.key, now);
                e.download_url = s.url;
                e.url_expires_at = Some(ts(s.expires_ms));
            }
        }
        Ok(e)
    }

    /// Pending exports for the export worker (marked running as returned).
    pub async fn claim_exports(
        &self,
        cx: &Cx,
        limit: usize,
    ) -> Result<Vec<(String, String, bool, bool)>> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let rows = tx
            .scan(
                &EXPORTS,
                &Scan {
                    scope: None,
                    order: Order::N1,
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        let mut out = Vec::new();
        for mut row in rows {
            let mut rec: ExportRec = row.decode()?;
            let Some(mut e) = rec.export.clone() else {
                continue;
            };
            if e.state != UserDataExportState::Pending as i32 {
                continue;
            }
            e.state = UserDataExportState::Running as i32;
            out.push((
                row.scope.clone(),
                e.id.clone(),
                rec.include_repos,
                rec.include_attachments,
            ));
            rec.export = Some(e);
            row.body = rec.encode_to_vec();
            tx.update(&EXPORTS, &row).await?;
            if out.len() >= limit {
                break;
            }
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Records the finished archive of an export (`Ok`) or its failure.
    pub async fn complete_export(
        &self,
        principal_id: &str,
        id: &str,
        result: std::result::Result<(u64, Vec<u8>), String>,
    ) -> Result<()> {
        let cx = Cx::system();
        let mut tx = self.db.begin(USERS, &cx).await?;
        let Some(mut row) = tx.get_for_update(&EXPORTS, principal_id, id).await? else {
            return Err(ForgeError::not_found(format!("data export {id}")));
        };
        let mut rec: ExportRec = row.decode()?;
        let mut e = rec.export.clone().unwrap_or_default();
        let now = tx.now_ms();
        match result {
            Ok((size, sha)) => {
                e.state = UserDataExportState::Ready as i32;
                e.size = size;
                e.sha256 = sha;
                e.completed_at = Some(ts(now));
                rec.key = format!("exports/{principal_id}/{id}");
            }
            Err(why) => {
                e.state = UserDataExportState::Failed as i32;
                e.error = why;
            }
        }
        rec.export = Some(e);
        row.body = rec.encode_to_vec();
        tx.update(&EXPORTS, &row).await?;
        tx.commit().await
    }

    /// Deletes the caller's account: hidden at once, purgeable after [`PURGE_DAYS`].
    pub async fn delete_account(&self, cx: &Cx, confirm_login: &str, reason: &str) -> Result<u64> {
        let (rec0, _) = self.ensure(cx, &cx.actor_id).await?;
        let login = rec0.profile.map(|p| p.login).unwrap_or_default();
        if !confirm_login.eq_ignore_ascii_case(&login) {
            return Err(ForgeError::invalid(
                "confirm_mismatch",
                "confirm_login must equal your login",
            ));
        }
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some(mut row) = tx.get_for_update(&PROFILES, "", &cx.actor_id).await? else {
            return Err(ForgeError::not_found("profile"));
        };
        let mut rec: ProfileRec = row.decode()?;
        if rec.deleted_ms > 0 {
            return Err(ForgeError::invalid_state("the account is already deleted"));
        }
        let now = tx.now_ms();
        rec.deleted_ms = now;
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        let purge = now + PURGE_DAYS * DAY_MS;
        Self::security(
            &mut tx,
            &cx.actor_id,
            cx,
            "account.delete",
            serde_json::json!({"reason": reason}),
        )
        .await?;
        Self::announce(
            &mut tx,
            "user.deleted",
            "user.delete",
            &cx.actor_id,
            serde_json::json!({"login": login, "reason": reason, "purge_after_ms": purge}),
        )
        .await?;
        tx.commit().await?;
        Ok(purge)
    }

    /// Restores a deleted account before its purge time (support tooling).
    pub async fn restore_account(&self, cx: &Cx, principal_id: &str) -> Result<()> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let Some(mut row) = tx.get_for_update(&PROFILES, "", principal_id).await? else {
            return Err(ForgeError::not_found(format!("user {principal_id}")));
        };
        let mut rec: ProfileRec = row.decode()?;
        if rec.deleted_ms == 0 {
            return Err(ForgeError::invalid_state("the account is not deleted"));
        }
        if rec.deleted_ms + PURGE_DAYS * DAY_MS <= tx.now_ms() {
            return Err(
                ForgeError::invalid_state("the account passed its purge time")
                    .with_subcode("purged"),
            );
        }
        rec.deleted_ms = 0;
        row.body = rec.encode_to_vec();
        tx.update(&PROFILES, &row).await?;
        Self::announce(
            &mut tx,
            "user.restored",
            "user.restore",
            principal_id,
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await
    }

    /// Removes the rows of accounts past their purge time and expired login redirects.
    pub async fn purge_expired(&self, cx: &Cx) -> Result<usize> {
        let mut tx = self.db.begin(USERS, cx).await?;
        let now = tx.now_ms();
        let mut n = 0;
        for row in tx
            .scan(
                &PROFILES,
                &Scan {
                    scope: Some(String::new()),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        {
            let rec: ProfileRec = row.decode()?;
            if rec.deleted_ms > 0 && rec.deleted_ms + PURGE_DAYS * DAY_MS <= now {
                let login = rec
                    .profile
                    .map(|p| p.login.to_lowercase())
                    .unwrap_or_default();
                tx.delete(&PROFILES, "", &row.id).await?;
                tx.delete(&LOGINS, "", &login).await?;
                n += 1;
            }
        }
        for row in tx
            .scan(
                &LOGINS,
                &Scan {
                    scope: Some(String::new()),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        {
            if row.n1.is_some_and(|e| e as u64 <= now) {
                tx.delete(&LOGINS, "", &row.id).await?;
            }
        }
        tx.commit().await?;
        Ok(n)
    }

    /// The pinned-item repo reference of a repo id.
    pub fn repo_ref(id: &str) -> Option<bee_proto::v1::RepoRef> {
        repo_ref_id(id)
    }
}

#[async_trait]
impl EmailDirectory for Users {
    async fn primary_verified(&self, principal_id: &str) -> Option<String> {
        let cx = Cx::system();
        let mut tx = self.db.begin(USERS, &cx).await.ok()?;
        let rows = tx
            .scan_msgs::<EmailRec>(&EMAILS, &Scan::scope(principal_id, 100))
            .await
            .ok()?;
        rows.into_iter()
            .filter_map(|(r, _)| r.email)
            .find(|e| e.verified && e.primary)
            .map(|e| e.email)
    }

    async fn verified(&self, principal_id: &str) -> Vec<String> {
        let cx = Cx::system();
        let Ok(mut tx) = self.db.begin(USERS, &cx).await else {
            return Vec::new();
        };
        tx.scan_msgs::<EmailRec>(&EMAILS, &Scan::scope(principal_id, 100))
            .await
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|(r, _)| r.email)
                    .filter(|e| e.verified)
                    .map(|e| e.email)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_and_emails_validate() {
        assert!(validate_login("octo-cat1").is_ok());
        for bad in ["", "-a", "a-", "a--b", "a_b", "a b"] {
            assert!(validate_login(bad).is_err(), "{bad}");
        }
        assert_eq!(validate_email(" A@B.example ").unwrap(), "a@b.example");
        for bad in ["a", "a@b", "@b.c", "a@b..", "a b@c.d"] {
            assert!(validate_email(bad).is_err(), "{bad}");
        }
    }
}
