//! Installations, installation tokens and the access source that ties repo roles to them.

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use bee_auth::{Attenuation, MintRequest, Principal as AuthPrincipal, Scope};
use bee_proto::v1::{
    App, AppInstallation, AppInstallationTargetKind, AppPermission, AppRepositorySelection, Repo,
    RepoRole, SubjectKind,
};

use super::catalog::{self, Permissions};
use super::registry::{load_app, page_rows};
use super::{
    Account, AccountKind, AppRec, Apps, CODES, DEVICE, HOOK_LINKS, HookLink, INDEX, INSTALL_REPOS,
    INSTALLS, InstallRec, OTOKENS, REFRESH, SURFACE, TOKENS, TokenRec, tsm,
};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::enterprise::common::{TokenRevoker, emit, get_or_404, json, scan_all};
use crate::error::{ForgeError, Result};
use crate::orgs::AccessSource;
use crate::store::{Db, Key, Order, Row, Scan, Tx};

/// Default and maximum installation token lifetime.
pub const MAX_TOKEN_TTL_SECS: u32 = 3600;
/// Most repos one token may carry explicitly.
pub const MAX_TOKEN_REPOS: usize = 500;
/// Most selected repos one installation may hold.
pub const MAX_SELECTED_REPOS: usize = 5000;

/// Writes each revocation before the lifecycle transaction commits. A failed store write is
/// retried twice so callers can safely retry an operation that ultimately returns unavailable.
pub(super) async fn revoke_before_commit(revoker: &dyn TokenRevoker, ids: &[String]) -> Result<()> {
    for id in ids {
        let mut last_error = None;
        for delay_ms in [0, 50, 200] {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            match revoker.revoke(id).await {
                Ok(()) => {
                    last_error = None;
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        if let Some(error) = last_error {
            return Err(ForgeError::unavailable(format!(
                "token revocation store: {}",
                error.message
            )));
        }
    }
    Ok(())
}

/// A request for an installation token.
#[derive(Clone, Debug, Default)]
pub struct TokenRequest {
    /// Repo ids to narrow to (empty = every repo of the installation).
    pub repo_ids: Vec<String>,
    /// Permissions to narrow to (empty = the installation's grant).
    pub permissions: Vec<AppPermission>,
    /// Lifetime in seconds (0 = 3600; at most 3600).
    pub ttl_seconds: u32,
}

/// A minted installation token.
#[derive(Clone, Debug)]
pub struct InstallationToken {
    /// The Biscuit, attenuated to the installation grant and the request.
    pub token: String,
    /// Its id (revoking it, or the root it derives from, kills it).
    pub token_id: String,
    /// Expiry in ms.
    pub expires_at_ms: u64,
    /// The explicit repo ids the token allows.
    pub repo_ids: Vec<String>,
    /// The effective permissions.
    pub permissions: Vec<AppPermission>,
}

/// What dropping an installation's rows leaves to do outside the transaction.
pub(crate) struct Removed {
    pub token_ids: Vec<String>,
    pub hooks: Vec<HookLink>,
}

/// Roles installation principals and app principals hold through installations: the weakest
/// built-in role covering the granted permissions, on exactly the covered repos, while the
/// installation is active.
pub struct AppsAccess {
    db: Db,
}

impl AppsAccess {
    /// An access source over the apps tables.
    pub fn new(db: Db) -> AppsAccess {
        AppsAccess { db }
    }
}

async fn role_on(tx: &mut Tx, rec: &InstallRec, repo: &Repo) -> Result<Option<(RepoRole, String)>> {
    let Some(inst) = &rec.inst else {
        return Ok(None);
    };
    if inst.suspended
        || inst.target_kind != AppInstallationTargetKind::Org as i32
        || rec.target_id != repo.org_id
    {
        return Ok(None);
    }
    let covered = match AppRepositorySelection::try_from(inst.repository_selection) {
        Ok(AppRepositorySelection::All) => true,
        Ok(AppRepositorySelection::Selected) => {
            tx.get(&INSTALL_REPOS, &inst.id, &repo.id).await?.is_some()
        }
        _ => false,
    };
    if !covered {
        return Ok(None);
    }
    let perms = catalog::from_proto(&inst.permissions, "bad_permission")?;
    let role = catalog::role_for(&catalog::actions_of(&perms));
    if role == RepoRole::None {
        return Ok(None);
    }
    Ok(Some((role, format!("app:{}", inst.app_slug))))
}

#[async_trait]
impl AccessSource for AppsAccess {
    async fn grants(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<Vec<(RepoRole, String)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if let Some((rec, _)) = tx
            .get_msg::<InstallRec>(&INSTALLS, "", principal_id)
            .await?
        {
            return Ok(role_on(&mut tx, &rec, repo).await?.into_iter().collect());
        }
        let Some(link) = tx.get(&INDEX, "principal", principal_id).await? else {
            return Ok(Vec::new());
        };
        let app_id = link.k1.unwrap_or_default();
        let target = format!("org:{}", repo.org_id);
        let rows = tx
            .scan_msgs::<InstallRec>(&INSTALLS, &Scan::scope("", 1000).key(Key::K2(target)))
            .await?;
        let mut best: Option<(RepoRole, String)> = None;
        for (rec, row) in rows {
            if row.k1.as_deref() != Some(app_id.as_str()) {
                continue;
            }
            if let Some(g) = role_on(&mut tx, &rec, repo).await?
                && best
                    .as_ref()
                    .is_none_or(|(r, _)| crate::orgs::role_rank(g.0) > crate::orgs::role_rank(*r))
            {
                best = Some(g);
            }
        }
        Ok(best.into_iter().collect())
    }
}

/// Removes an installation's rows (repos, hook links, token rows, the row itself), records the
/// uninstall event and audit entry, and returns the tokens to revoke and hooks to delete.
pub(crate) async fn drop_installation(
    tx: &mut Tx,
    rec: &InstallRec,
    reason: &str,
) -> Result<Removed> {
    let inst = rec.inst.clone().unwrap_or_default();
    let mut token_ids = Vec::new();
    for (_, row) in scan_all::<TokenRec>(tx, &TOKENS, &inst.id, None).await? {
        token_ids.push(row.id.clone());
        tx.delete(&TOKENS, &inst.id, &row.id).await?;
    }
    let mut hooks = Vec::new();
    for (link, row) in scan_all::<HookLink>(tx, &HOOK_LINKS, &inst.id, None).await? {
        hooks.push(link);
        tx.delete(&HOOK_LINKS, &inst.id, &row.id).await?;
    }
    for (_, row) in scan_all::<TokenRec>(tx, &INSTALL_REPOS, &inst.id, None).await? {
        tx.delete(&INSTALL_REPOS, &inst.id, &row.id).await?;
    }
    tx.delete(&INSTALLS, "", &inst.id).await?;
    let org = org_of(rec);
    let subject = (!org.is_empty()).then_some((SubjectKind::Org, org.as_str()));
    emit(tx, "app.uninstalled", &org, subject, json(&inst)).await?;
    let mut after = serde_json::json!({ "reason": reason });
    after["installation_id"] = inst.id.clone().into();
    tx.audit(
        AuditDraft::new("app.uninstall", &org)
            .target_id(&inst.id)
            .before(&json(&inst))
            .after(&after.to_string()),
    )
    .await?;
    Ok(Removed { token_ids, hooks })
}

fn org_of(rec: &InstallRec) -> String {
    rec.target_key
        .strip_prefix("org:")
        .unwrap_or_default()
        .to_string()
}

/// After an app's requested permissions changed: clamp every installation's grant to what is
/// still requested and flag installations that now lack something the app asks for.
pub(crate) async fn reconcile_permissions(
    tx: &mut Tx,
    app_id: &str,
    requested: &Permissions,
    now: u64,
) -> Result<()> {
    let rows = scan_all::<InstallRec>(tx, &INSTALLS, "", Some(Key::K1(app_id.to_string()))).await?;
    for (mut rec, row) in rows {
        let Some(mut inst) = rec.inst.clone() else {
            continue;
        };
        let held = catalog::from_proto(&inst.permissions, "bad_permission")?;
        let mut kept = Permissions::new();
        for (r, l) in &held {
            if let Some(req) = requested.get(r) {
                kept.insert(r.clone(), (*l).min(*req));
            }
        }
        let kept = catalog::with_metadata(kept);
        inst.permissions = catalog::to_proto(&kept);
        inst.permissions_update_pending = !catalog::covers(&kept, requested);
        inst.updated_at = tsm(now);
        rec.inst = Some(inst);
        tx.update(&INSTALLS, &row.clone().msg(&rec)).await?;
    }
    Ok(())
}

fn view(rec: &InstallRec, app: &App) -> AppInstallation {
    let mut i = rec.inst.clone().unwrap_or_default();
    i.app_slug = app.slug.clone();
    i.events = app.events.clone();
    i
}

async fn app_of(tx: &mut Tx, app_id: &str) -> Result<(AppRec, Row)> {
    load_app(tx, app_id).await
}

impl Apps {
    async fn fill_target(&self, cx: &Cx, i: &mut AppInstallation, key: &str) {
        i.target = self.account_name(cx, key).await;
    }

    fn check_repos(target: &Account, repos: &[Repo]) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for r in repos {
            if target.kind != AccountKind::Org || r.org_id != target.id {
                return Err(ForgeError::invalid(
                    "repo_not_in_account",
                    format!("repo {} does not belong to {}", r.full_name, target.name),
                ));
            }
            out.insert(r.id.clone());
        }
        Ok(out)
    }

    /// Installs an app on an account. `repos` are the selected repos (they must belong to the
    /// target org). Emits `app.installed` and syncs the app's webhooks.
    pub async fn create_installation(
        &self,
        cx: &Cx,
        app_by: &str,
        target: &Account,
        selection: AppRepositorySelection,
        repos: &[Repo],
    ) -> Result<AppInstallation> {
        let selected = match selection {
            AppRepositorySelection::All => {
                if !repos.is_empty() {
                    return Err(ForgeError::invalid(
                        "repos_with_all",
                        "repos are only for SELECTED installations",
                    ));
                }
                BTreeSet::new()
            }
            AppRepositorySelection::Selected => Self::check_repos(target, repos)?,
            AppRepositorySelection::Unspecified => {
                return Err(ForgeError::missing("repository_selection"));
            }
        };
        if selected.len() > MAX_SELECTED_REPOS {
            return Err(ForgeError::too_large(
                "selected_repos",
                MAX_SELECTED_REPOS as u64,
                selected.len() as u64,
            ));
        }
        let id = self.db.ids().ulid()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (app_rec, _) = app_of(&mut tx, app_by).await?;
        let app = app_rec.app.clone().unwrap_or_default();
        let existing = tx
            .scan(&INSTALLS, &Scan::scope("", 1000).key(Key::K2(target.key())))
            .await?;
        if existing
            .iter()
            .any(|r| r.k1.as_deref() == Some(app.id.as_str()))
        {
            return Err(ForgeError::already_exists(format!(
                "installation of {} on {}",
                app.slug, target.name
            )));
        }
        let inst = AppInstallation {
            id: id.clone(),
            app_id: app.id.clone(),
            app_slug: app.slug.clone(),
            target: target.name.clone(),
            target_kind: match target.kind {
                AccountKind::Org => AppInstallationTargetKind::Org,
                AccountKind::User => AppInstallationTargetKind::User,
            } as i32,
            repository_selection: selection as i32,
            permissions: app.permissions.clone(),
            events: app.events.clone(),
            permissions_update_pending: false,
            principal_id: id.clone(),
            installed_by: Some(cx.principal()),
            suspended: false,
            suspended_by: None,
            suspended_at: None,
            created_at: tsm(now),
            updated_at: tsm(now),
        };
        let rec = InstallRec {
            inst: Some(inst.clone()),
            target_key: target.key(),
            target_id: target.id.clone(),
            suspend_reason: String::new(),
        };
        tx.insert(
            &INSTALLS,
            &Row::new("", id.clone())
                .k1(app.id.clone())
                .k2(target.key())
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        for r in &selected {
            tx.insert(
                &INSTALL_REPOS,
                &Row::new(id.clone(), r.clone()).n1(now as i64),
            )
            .await?;
        }
        let org = target.org_id().to_string();
        let subject = (!org.is_empty()).then_some((SubjectKind::Org, org.as_str()));
        emit(&mut tx, "app.installed", &org, subject, json(&inst)).await?;
        tx.audit(
            AuditDraft::new("app.install", &org)
                .target_id(&id)
                .after(&json(&inst)),
        )
        .await?;
        tx.commit().await?;
        if let Err(e) = self.resync_installation_hooks(cx, &id).await {
            // Do not leave an installation whose webhooks could not be set up.
            let _ = self.delete_installation(cx, &id).await;
            return Err(e);
        }
        let mut out = view(&rec, &app);
        out.target = target.name.clone();
        Ok(out)
    }

    async fn load_installation(&self, cx: &Cx, id: &str) -> Result<(InstallRec, App)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = get_or_404::<InstallRec>(&mut tx, &INSTALLS, "", id, "installation").await?;
        let app_id = rec
            .inst
            .as_ref()
            .map(|i| i.app_id.clone())
            .unwrap_or_default();
        let (app_rec, _) = app_of(&mut tx, &app_id).await?;
        Ok((rec, app_rec.app.unwrap_or_default()))
    }

    /// Reads one installation.
    pub async fn get_installation(&self, cx: &Cx, id: &str) -> Result<AppInstallation> {
        let (rec, app) = self.load_installation(cx, id).await?;
        let mut v = view(&rec, &app);
        self.fill_target(cx, &mut v, &rec.target_key).await;
        Ok(v)
    }

    /// The target key (`org:<id>` / `user:<id>`) and app id of an installation.
    pub async fn installation_target(&self, cx: &Cx, id: &str) -> Result<(String, String)> {
        let (rec, app) = self.load_installation(cx, id).await?;
        Ok((rec.target_key, app.id))
    }

    async fn views(&self, cx: &Cx, rows: Vec<(InstallRec, Row)>) -> Result<Vec<AppInstallation>> {
        let mut out = Vec::new();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut apps: std::collections::BTreeMap<String, App> = Default::default();
        let mut pending = Vec::new();
        for (rec, _) in rows {
            let app_id = rec
                .inst
                .as_ref()
                .map(|i| i.app_id.clone())
                .unwrap_or_default();
            if !apps.contains_key(&app_id) {
                let (a, _) = app_of(&mut tx, &app_id).await?;
                apps.insert(app_id.clone(), a.app.unwrap_or_default());
            }
            pending.push((view(&rec, &apps[&app_id]), rec.target_key));
        }
        drop(tx);
        for (mut v, key) in pending {
            self.fill_target(cx, &mut v, &key).await;
            out.push(v);
        }
        Ok(out)
    }

    /// Installations of an app, by id, paged.
    pub async fn list_installations_of_app(
        &self,
        cx: &Cx,
        app_by: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<AppInstallation>, String)> {
        let (rows, next) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let (app, _) = app_of(&mut tx, app_by).await?;
            let app_id = app.app.map(|a| a.id).unwrap_or_default();
            let (rows, next) =
                page_rows(&mut tx, &INSTALLS, "", Some(Key::K1(app_id)), after, limit).await?;
            let decoded = rows
                .into_iter()
                .map(|r| Ok((r.decode::<InstallRec>()?, r)))
                .collect::<Result<Vec<_>>>()?;
            (decoded, next)
        };
        Ok((self.views(cx, rows).await?, next))
    }

    /// Apps installed on an account, by installation id, paged.
    pub async fn list_installations_of_account(
        &self,
        cx: &Cx,
        account: &Account,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<AppInstallation>, String)> {
        let (rows, next) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let (rows, next) = page_rows(
                &mut tx,
                &INSTALLS,
                "",
                Some(Key::K2(account.key())),
                after,
                limit,
            )
            .await?;
            let decoded = rows
                .into_iter()
                .map(|r| Ok((r.decode::<InstallRec>()?, r)))
                .collect::<Result<Vec<_>>>()?;
            (decoded, next)
        };
        Ok((self.views(cx, rows).await?, next))
    }

    /// Suspends an installation: its tracked tokens are revoked, it grants nothing and its
    /// webhooks stop.
    pub async fn suspend_installation(
        &self,
        cx: &Cx,
        id: &str,
        reason: &str,
    ) -> Result<AppInstallation> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (mut rec, row) =
            get_or_404::<InstallRec>(&mut tx, &INSTALLS, "", id, "installation").await?;
        let mut inst = rec.inst.clone().unwrap_or_default();
        if inst.suspended {
            return Err(ForgeError::invalid_state(
                "installation is already suspended",
            ));
        }
        let before = json(&inst);
        inst.suspended = true;
        inst.suspended_by = Some(cx.principal());
        inst.suspended_at = tsm(now);
        inst.updated_at = tsm(now);
        rec.inst = Some(inst.clone());
        rec.suspend_reason = reason.to_string();
        tx.update(&INSTALLS, &row.clone().msg(&rec)).await?;
        let mut revoke = Vec::new();
        for (_, r) in scan_all::<TokenRec>(&mut tx, &TOKENS, id, None).await? {
            revoke.push(r.id.clone());
            tx.delete(&TOKENS, id, &r.id).await?;
        }
        let org = org_of(&rec);
        let subject = (!org.is_empty()).then_some((SubjectKind::Org, org.as_str()));
        emit(&mut tx, "app.suspended", &org, subject, json(&inst)).await?;
        tx.audit(
            AuditDraft::new("app.suspend", &org)
                .target_id(id)
                .before(&before)
                .after(&json(&inst)),
        )
        .await?;
        let (_, app_row) = (0, app_of(&mut tx, &inst.app_id).await?);
        revoke_before_commit(self.cfg.revoker.as_ref(), &revoke).await?;
        tx.commit().await?;
        self.resync_installation_hooks(cx, id).await?;
        let app = app_row.0.app.unwrap_or_default();
        let mut v = view(&rec, &app);
        self.fill_target(cx, &mut v, &rec.target_key).await;
        Ok(v)
    }

    /// Lifts a suspension (revoked tokens stay revoked; mint new ones).
    pub async fn unsuspend_installation(&self, cx: &Cx, id: &str) -> Result<AppInstallation> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (mut rec, row) =
            get_or_404::<InstallRec>(&mut tx, &INSTALLS, "", id, "installation").await?;
        let mut inst = rec.inst.clone().unwrap_or_default();
        if !inst.suspended {
            return Err(ForgeError::invalid_state("installation is not suspended"));
        }
        let before = json(&inst);
        inst.suspended = false;
        inst.suspended_by = None;
        inst.suspended_at = None;
        inst.updated_at = tsm(now);
        rec.inst = Some(inst.clone());
        rec.suspend_reason.clear();
        tx.update(&INSTALLS, &row.clone().msg(&rec)).await?;
        let org = org_of(&rec);
        let subject = (!org.is_empty()).then_some((SubjectKind::Org, org.as_str()));
        emit(&mut tx, "app.unsuspended", &org, subject, json(&inst)).await?;
        tx.audit(
            AuditDraft::new("app.unsuspend", &org)
                .target_id(id)
                .before(&before)
                .after(&json(&inst)),
        )
        .await?;
        let app = app_of(&mut tx, &inst.app_id)
            .await?
            .0
            .app
            .unwrap_or_default();
        tx.commit().await?;
        self.resync_installation_hooks(cx, id).await?;
        let mut v = view(&rec, &app);
        self.fill_target(cx, &mut v, &rec.target_key).await;
        Ok(v)
    }

    /// Uninstalls an app: rows, tracked tokens (revoked) and webhooks go away and
    /// `app.uninstalled` is emitted.
    pub async fn delete_installation(&self, cx: &Cx, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = get_or_404::<InstallRec>(&mut tx, &INSTALLS, "", id, "installation").await?;
        let removed = drop_installation(&mut tx, &rec, "uninstalled").await?;
        revoke_before_commit(self.cfg.revoker.as_ref(), &removed.token_ids).await?;
        tx.commit().await?;
        self.delete_hooks(cx, removed.hooks).await;
        Ok(())
    }

    /// Adds repos to a `SELECTED` installation; returns the number of repos it covers.
    pub async fn add_installation_repos(&self, cx: &Cx, id: &str, repos: &[Repo]) -> Result<u32> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (rec, _) = get_or_404::<InstallRec>(&mut tx, &INSTALLS, "", id, "installation").await?;
        let inst = rec.inst.clone().unwrap_or_default();
        Self::require_selected(&inst)?;
        let target = Account {
            kind: AccountKind::Org,
            id: rec.target_id.clone(),
            name: inst.target.clone(),
        };
        let add = Self::check_repos(&target, repos)?;
        let have = tx.count(&INSTALL_REPOS, id, None).await? as usize;
        let mut count = have;
        for r in &add {
            if tx.get(&INSTALL_REPOS, id, r).await?.is_none() {
                if count >= MAX_SELECTED_REPOS {
                    return Err(ForgeError::too_large(
                        "selected_repos",
                        MAX_SELECTED_REPOS as u64,
                        count as u64 + 1,
                    ));
                }
                tx.insert(&INSTALL_REPOS, &Row::new(id, r.clone()).n1(now as i64))
                    .await?;
                count += 1;
            }
        }
        let org = org_of(&rec);
        tx.audit(
            AuditDraft::new("app.installation_repos_add", &org)
                .target_id(id)
                .after(&serde_json::json!({ "repos": add }).to_string()),
        )
        .await?;
        tx.commit().await?;
        self.resync_installation_hooks(cx, id).await?;
        Ok(count as u32)
    }

    fn require_selected(inst: &AppInstallation) -> Result<()> {
        if inst.repository_selection != AppRepositorySelection::Selected as i32 {
            return Err(ForgeError::invalid_state(
                "the installation covers all repos; only SELECTED installations take repo changes",
            ));
        }
        Ok(())
    }

    /// Removes repos from a `SELECTED` installation; returns the number of repos it covers.
    pub async fn remove_installation_repos(
        &self,
        cx: &Cx,
        id: &str,
        repos: &[Repo],
    ) -> Result<u32> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = get_or_404::<InstallRec>(&mut tx, &INSTALLS, "", id, "installation").await?;
        let inst = rec.inst.clone().unwrap_or_default();
        Self::require_selected(&inst)?;
        let mut gone = Vec::new();
        for r in repos {
            if tx.delete(&INSTALL_REPOS, id, &r.id).await? {
                gone.push(r.id.clone());
            }
        }
        let count = tx.count(&INSTALL_REPOS, id, None).await?;
        let org = org_of(&rec);
        tx.audit(
            AuditDraft::new("app.installation_repos_remove", &org)
                .target_id(id)
                .after(&serde_json::json!({ "repos": gone }).to_string()),
        )
        .await?;
        tx.commit().await?;
        self.resync_installation_hooks(cx, id).await?;
        Ok(count as u32)
    }

    /// The repo ids an installation covers right now (`ALL` enumerates the org's live repos).
    pub async fn installation_repo_ids(&self, cx: &Cx, id: &str) -> Result<BTreeSet<String>> {
        let (rec, _) = self.load_installation(cx, id).await?;
        self.covered_repo_ids(cx, &rec).await
    }

    pub(crate) async fn covered_repo_ids(
        &self,
        cx: &Cx,
        rec: &InstallRec,
    ) -> Result<BTreeSet<String>> {
        let Some(inst) = &rec.inst else {
            return Ok(BTreeSet::new());
        };
        if inst.target_kind != AppInstallationTargetKind::Org as i32 {
            return Ok(BTreeSet::new());
        }
        if inst.repository_selection == AppRepositorySelection::Selected as i32 {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let rows = tx
                .scan(
                    &INSTALL_REPOS,
                    &Scan::scope(inst.id.clone(), crate::store::MAX_SCAN),
                )
                .await?;
            return Ok(rows.into_iter().map(|r| r.id).collect());
        }
        let mut out = BTreeSet::new();
        let mut after = String::new();
        for _ in 0..200 {
            let (repos, next) = self
                .forge
                .orgs
                .list_repos(cx, &rec.target_id, &after, 500)
                .await?;
            out.extend(repos.into_iter().map(|r| r.id));
            if next.is_empty() {
                break;
            }
            after = next;
        }
        Ok(out)
    }

    /// Repo ids an installation covers, paged (for `ALL`, the org's live repos by id).
    pub async fn list_installation_repos(
        &self,
        cx: &Cx,
        id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<String>, String)> {
        let (rec, _) = self.load_installation(cx, id).await?;
        let inst = rec.inst.clone().unwrap_or_default();
        if inst.target_kind != AppInstallationTargetKind::Org as i32 {
            return Ok((Vec::new(), String::new()));
        }
        if inst.repository_selection == AppRepositorySelection::All as i32 {
            let (repos, next) = self
                .forge
                .orgs
                .list_repos(cx, &rec.target_id, after, limit)
                .await?;
            return Ok((repos.into_iter().map(|r| r.id).collect(), next));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) = page_rows(&mut tx, &INSTALL_REPOS, id, None, after, limit).await?;
        Ok((rows.into_iter().map(|r| r.id).collect(), next))
    }

    /// An installer accepts the app's current permission request: the installation's grant
    /// becomes the app's requested set and `permissions_update_pending` clears. (Tokens
    /// minted earlier keep the narrower scope they were minted with.)
    pub async fn accept_installation_permissions(
        &self,
        cx: &Cx,
        id: &str,
    ) -> Result<AppInstallation> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (mut rec, row) =
            get_or_404::<InstallRec>(&mut tx, &INSTALLS, "", id, "installation").await?;
        let mut inst = rec.inst.clone().unwrap_or_default();
        let app = app_of(&mut tx, &inst.app_id)
            .await?
            .0
            .app
            .unwrap_or_default();
        let before = json(&inst);
        inst.permissions = app.permissions.clone();
        inst.permissions_update_pending = false;
        inst.updated_at = tsm(now);
        rec.inst = Some(inst.clone());
        tx.update(&INSTALLS, &row.clone().msg(&rec)).await?;
        let org = org_of(&rec);
        tx.audit(
            AuditDraft::new("app.permissions_accept", &org)
                .target_id(id)
                .before(&before)
                .after(&json(&inst)),
        )
        .await?;
        tx.commit().await?;
        let mut v = view(&rec, &app);
        self.fill_target(cx, &mut v, &rec.target_key).await;
        Ok(v)
    }

    /// Mints an installation token: a Biscuit for the installation principal whose scope is
    /// the installation grant (an explicit repo id set and the actions of the granted
    /// permissions), attenuated to the requested subset. Asking for more than the
    /// installation grants is `PERMISSION_DENIED`; a suspended installation is
    /// `INVALID_STATE`; an IP allow list may answer `IP_NOT_ALLOWED`.
    pub async fn create_installation_token(
        &self,
        cx: &Cx,
        id: &str,
        req: TokenRequest,
    ) -> Result<InstallationToken> {
        let ttl = match req.ttl_seconds {
            0 => MAX_TOKEN_TTL_SECS,
            t if t > MAX_TOKEN_TTL_SECS => {
                return Err(ForgeError::invalid(
                    "ttl_too_long",
                    format!("ttl_seconds is at most {MAX_TOKEN_TTL_SECS}"),
                ));
            }
            t => t,
        };
        let (rec, app) = self.load_installation(cx, id).await?;
        let inst = rec.inst.clone().unwrap_or_default();
        if inst.suspended {
            return Err(ForgeError::invalid_state("the installation is suspended"));
        }
        if let Some(p) = &self.cfg.ip_policy
            && inst.target_kind == AppInstallationTargetKind::Org as i32
        {
            p.check_ip(cx, &rec.target_id, &cx.ip, true).await?;
        }
        let covered = self.covered_repo_ids(cx, &rec).await?;
        let requested: BTreeSet<String> = if req.repo_ids.is_empty() {
            covered.clone()
        } else {
            for r in &req.repo_ids {
                if !covered.contains(r) {
                    return Err(ForgeError::permission_denied(format!(
                        "repo {r} is not covered by the installation"
                    )));
                }
            }
            req.repo_ids.iter().cloned().collect()
        };
        let granted = catalog::from_proto(&inst.permissions, "bad_permission")?;
        let effective = if req.permissions.is_empty() {
            granted.clone()
        } else {
            let want = catalog::with_metadata(catalog::from_proto(
                &req.permissions,
                "invalid_permission",
            )?);
            if !catalog::covers(&granted, &want) {
                return Err(ForgeError::permission_denied(
                    "the installation does not grant the requested permissions",
                ));
            }
            want
        };
        if requested.len() > MAX_TOKEN_REPOS {
            return Err(ForgeError::too_large(
                "token_repos",
                MAX_TOKEN_REPOS as u64,
                requested.len() as u64,
            )
            .with_subcode("narrow_repos"));
        }
        // The root block is the installation grant; with a very large `ALL` installation the
        // explicit set would not fit, so the root then carries the requested subset.
        let root_repos = if covered.len() <= MAX_TOKEN_REPOS {
            &covered
        } else {
            &requested
        };
        let now = self.now_secs();
        let exp = now + u64::from(ttl);
        let root_scope = Scope::new()
            .repos(root_repos.iter())?
            .actions(catalog::actions_of(&granted))
            .expires_at(exp);
        let root = self.cfg.keys.mint(
            &MintRequest {
                principal: AuthPrincipal::service(
                    inst.principal_id.clone(),
                    format!("{}[bot]", app.slug),
                ),
                on_behalf_of: Vec::new(),
                scope: root_scope,
            },
            now,
        )?;
        let narrow = Scope::new()
            .repos(requested.iter())?
            .actions(catalog::actions_of(&effective))
            .expires_at(exp);
        let token = bee_auth::attenuate(
            &root.text,
            &Attenuation {
                scope: narrow,
                act_as: None,
            },
        )?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for tid in [&root.id.0, &token.id.0] {
            tx.put(
                &TOKENS,
                &Row::new(id, tid.clone())
                    .n1((exp * 1000) as i64)
                    .msg(&TokenRec {
                        expires_ms: exp * 1000,
                    }),
            )
            .await?;
        }
        tx.audit(
            AuditDraft::new("app.token_create", &org_of(&rec))
                .target_id(id)
                .after(
                    &serde_json::json!({
                        "token_id": token.id.0,
                        "repos": requested.len(),
                        "permissions": effective
                            .iter()
                            .map(|(r, l)| format!("{r}:{}", l.as_str()))
                            .collect::<Vec<_>>(),
                        "expires_at": exp,
                    })
                    .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(InstallationToken {
            token: token.text,
            token_id: token.id.0,
            expires_at_ms: exp * 1000,
            repo_ids: requested.into_iter().collect(),
            permissions: catalog::to_proto(&effective),
        })
    }

    /// Deletes expired token rows, codes, device flows and refresh tokens; returns how many.
    pub async fn sweep_expired(&self, cx: &Cx) -> Result<usize> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms() as i64;
        let mut n = 0;
        for t in [&TOKENS, &OTOKENS] {
            let rows = tx
                .scan(
                    t,
                    &Scan {
                        order: Order::N1,
                        n1_max: Some(now),
                        limit: 5000,
                        ..Scan::default()
                    },
                )
                .await?;
            for r in rows {
                tx.delete(t, &r.scope, &r.id).await?;
                n += 1;
            }
        }
        for t in [&CODES, &DEVICE, &REFRESH] {
            let rows = tx
                .scan(
                    t,
                    &Scan {
                        scope: Some(String::new()),
                        order: Order::N1,
                        n1_max: Some(now),
                        limit: 5000,
                        ..Scan::default()
                    },
                )
                .await?;
            for r in rows {
                tx.delete(t, "", &r.id).await?;
                n += 1;
            }
        }
        tx.commit().await?;
        Ok(n)
    }
}
