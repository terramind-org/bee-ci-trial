//! Apps: manifests, private keys, app JWT authentication.

use std::collections::BTreeSet;

use bee_proto::v1::{App, AppPrivateKey, ErrorReason, PrincipalKind};

use super::catalog::{self, Permissions};
use super::crypto;
use super::{APPS, Account, AppRec, Apps, INDEX, INSTALLS, KEYS, KeyRec, SURFACE, tsm};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::enterprise::common::{
    MAX_LIST, get_or_404, json, page_cursor, scan_all, sha256_hex, slugify, valid_slug,
};
use crate::error::{ForgeError, Result};
use crate::store::{Key, Row, Scan, TableDef, Tx};

/// Most apps one account owns.
pub const MAX_APPS_PER_OWNER: u64 = 100;
/// Most private keys one app holds.
pub const MAX_KEYS_PER_APP: u64 = 10;

/// What creating an app returns; the secrets appear only here.
#[derive(Clone, Debug)]
pub struct NewApp {
    /// The app.
    pub app: App,
    /// The OAuth client secret.
    pub client_secret: String,
    /// The webhook HMAC secret.
    pub webhook_secret: String,
    /// The first private key, PKCS#8 PEM.
    pub private_key_pem: String,
    /// Its id (the JWT `kid`).
    pub private_key_id: String,
}

/// A partial app update; `None` leaves a field alone.
#[derive(Clone, Debug, Default)]
pub struct AppPatch {
    /// Display name.
    pub name: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Homepage URL.
    pub homepage_url: Option<String>,
    /// OAuth callback URLs (replaces the list).
    pub callback_urls: Option<Vec<String>>,
    /// Post-install URL.
    pub setup_url: Option<String>,
    /// Ask installers to authorize the app too.
    pub request_oauth_on_install: Option<bool>,
    /// Webhook URL.
    pub webhook_url: Option<String>,
    /// Webhook on or off.
    pub webhook_active: Option<bool>,
    /// New webhook secret.
    pub webhook_secret: Option<String>,
    /// Requested permissions (replaces the set).
    pub permissions: Option<Permissions>,
    /// Subscribed event kinds (replaces the list).
    pub events: Option<Vec<String>>,
    /// Installable by any account.
    pub public: Option<bool>,
}

#[derive(Debug, Default)]
struct Manifest {
    name: String,
    description: String,
    url: String,
    hook_url: String,
    hook_active: bool,
    callback_urls: Vec<String>,
    setup_url: String,
    public: bool,
    request_oauth_on_install: bool,
    permissions: Permissions,
    events: Vec<String>,
}

fn bad(m: impl Into<String>) -> ForgeError {
    ForgeError::invalid("bad_manifest", m)
}

/// Validates a URL field (`http(s)://host...`).
pub(crate) fn check_url(field: &str, u: &str, subcode: &str) -> Result<()> {
    let ok = (u.starts_with("https://") || u.starts_with("http://"))
        && u.len() <= 2048
        && u.split("://").nth(1).is_some_and(|rest| !rest.is_empty())
        && !u.chars().any(|c| c.is_whitespace() || c.is_control())
        && !u.contains('#');
    if ok {
        Ok(())
    } else {
        Err(ForgeError::invalid(
            subcode,
            format!("`{field}` is not a valid http(s) URL"),
        ))
    }
}

fn check_urls(field: &str, list: &[String], subcode: &str) -> Result<()> {
    if list.len() > 10 {
        return Err(ForgeError::invalid(
            subcode,
            format!("`{field}` has more than 10 entries"),
        ));
    }
    list.iter().try_for_each(|u| check_url(field, u, subcode))
}

fn check_events(apps: &Apps, events: &[String], subcode: &str) -> Result<Vec<String>> {
    let mut set = BTreeSet::new();
    for e in events {
        if !apps.forge.registry.filter_known(e) {
            return Err(ForgeError::invalid(
                subcode,
                format!("unknown event kind {e:?}"),
            ));
        }
        set.insert(e.clone());
    }
    Ok(set.into_iter().collect())
}

fn parse_manifest(apps: &Apps, text: &str) -> Result<Manifest> {
    use serde_json::Value;
    let v: Value = serde_json::from_str(text).map_err(|e| bad(format!("not JSON: {e}")))?;
    let Value::Object(o) = v else {
        return Err(bad("the manifest must be a JSON object"));
    };
    const KNOWN: &[&str] = &[
        "name",
        "description",
        "url",
        "hook_attributes",
        "redirect_url",
        "callback_urls",
        "setup_url",
        "public",
        "default_permissions",
        "default_events",
        "request_oauth_on_install",
    ];
    if let Some(k) = o.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(bad(format!("unknown manifest field {k:?}")));
    }
    let str_of = |k: &str| -> Result<String> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(bad(format!("`{k}` must be a string"))),
        }
    };
    let bool_of = |k: &str, default: bool| -> Result<bool> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(bad(format!("`{k}` must be a boolean"))),
        }
    };
    let list_of = |k: &str| -> Result<Vec<String>> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(a)) => a
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| bad(format!("`{k}` must be a list of strings")))
                })
                .collect(),
            Some(_) => Err(bad(format!("`{k}` must be a list of strings"))),
        }
    };
    let mut m = Manifest {
        name: str_of("name")?.trim().to_string(),
        description: str_of("description")?,
        url: str_of("url")?,
        setup_url: str_of("setup_url")?,
        public: bool_of("public", false)?,
        request_oauth_on_install: bool_of("request_oauth_on_install", false)?,
        hook_active: false,
        ..Manifest::default()
    };
    if m.name.is_empty() || m.name.chars().count() > 34 {
        return Err(bad("`name` is required and at most 34 characters"));
    }
    if slugify(&m.name).is_empty() {
        return Err(bad("`name` needs letters or digits"));
    }
    if m.description.chars().count() > 1000 {
        return Err(bad("`description` is longer than 1000 characters"));
    }
    if m.url.is_empty() {
        return Err(bad("`url` is required"));
    }
    check_url("url", &m.url, "bad_manifest")?;
    if !m.setup_url.is_empty() {
        check_url("setup_url", &m.setup_url, "bad_manifest")?;
    }
    match o.get("hook_attributes") {
        None | Some(Value::Null) => {}
        Some(Value::Object(h)) => {
            m.hook_url = h
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            m.hook_active = match h.get("active") {
                None | Some(Value::Null) => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(bad("`hook_attributes.active` must be a boolean")),
            };
            if m.hook_url.is_empty() {
                return Err(bad("`hook_attributes.url` is required"));
            }
            check_url("hook_attributes.url", &m.hook_url, "bad_manifest")?;
        }
        Some(_) => return Err(bad("`hook_attributes` must be an object")),
    }
    m.callback_urls = list_of("callback_urls")?;
    let redirect = str_of("redirect_url")?;
    if !redirect.is_empty() && !m.callback_urls.contains(&redirect) {
        m.callback_urls.push(redirect);
    }
    check_urls("callback_urls", &m.callback_urls, "bad_manifest")?;
    match o.get("default_permissions") {
        None | Some(Value::Null) => {}
        Some(Value::Object(p)) => {
            let mut list = Vec::new();
            for (resource, level) in p {
                let Some(l) = level.as_str().and_then(catalog::Level::parse) else {
                    return Err(bad(format!(
                        "permission {resource:?} must be \"read\", \"write\" or \"admin\""
                    )));
                };
                list.push(bee_proto::v1::AppPermission {
                    resource: resource.clone(),
                    level: l.to_proto() as i32,
                });
            }
            m.permissions = catalog::from_proto(&list, "bad_manifest")?;
        }
        Some(_) => return Err(bad("`default_permissions` must be an object")),
    }
    m.permissions = catalog::with_metadata(m.permissions);
    m.events = check_events(apps, &list_of("default_events")?, "bad_manifest")?;
    if !m.events.is_empty() && m.hook_url.is_empty() {
        return Err(bad("`default_events` need `hook_attributes.url`"));
    }
    Ok(m)
}

/// Reads one page of ids in a scope (rows only; used for index tables).
pub(crate) async fn page_rows(
    tx: &mut Tx,
    t: &TableDef,
    scope: &str,
    key: Option<Key>,
    after: &str,
    limit: usize,
) -> Result<(Vec<Row>, String)> {
    let limit = limit.clamp(1, MAX_LIST);
    let mut q = Scan::scope(scope, limit + 1).after(page_cursor(scope, after));
    if let Some(k) = key {
        q = q.key(k);
    }
    let mut rows = tx.scan(t, &q).await?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = if more {
        rows.last().map(|r| r.id.clone()).unwrap_or_default()
    } else {
        String::new()
    };
    Ok((rows, next))
}

/// Loads an app by id, slug or client id.
pub(crate) async fn load_app(tx: &mut Tx, by: &str) -> Result<(AppRec, Row)> {
    if by.is_empty() {
        return Err(ForgeError::missing("app"));
    }
    if let Some((rec, row)) = tx.get_msg::<AppRec>(&APPS, "", by).await? {
        return Ok((rec, row));
    }
    for key in [Key::K1(by.to_string()), Key::K2(by.to_string())] {
        let rows = tx.scan(&APPS, &Scan::scope("", 1).key(key)).await?;
        if let Some(row) = rows.into_iter().next() {
            return Ok((row.decode()?, row));
        }
    }
    Err(ForgeError::not_found(format!("app {by}")))
}

/// The public view of an app: keys and installation count filled in.
pub(crate) async fn app_view(tx: &mut Tx, rec: &AppRec) -> Result<App> {
    let mut app = rec.app.clone().unwrap_or_default();
    app.private_keys = scan_all::<KeyRec>(tx, &KEYS, &app.id, None)
        .await?
        .into_iter()
        .filter_map(|(k, _)| k.key)
        .collect();
    app.installation_count = tx
        .count(&INSTALLS, "", Some(&Key::K1(app.id.clone())))
        .await?;
    Ok(app)
}

fn keysum(id: &str, fp: &str, now: u64) -> AppPrivateKey {
    AppPrivateKey {
        id: id.to_string(),
        fingerprint: fp.to_string(),
        created_at: tsm(now),
        last_used_at: None,
    }
}

impl Apps {
    async fn fill_owner(&self, cx: &Cx, app: &mut App, owner_key: &str) {
        app.owner = self.account_name(cx, owner_key).await;
    }

    /// Creates an app from a JSON manifest (see the proto for its fields). The secrets in
    /// the result are shown once and stored only as hashes (the webhook secret excepted, see
    /// the module docs).
    pub async fn create_app(
        &self,
        cx: &Cx,
        owner: &Account,
        manifest_json: &str,
    ) -> Result<NewApp> {
        let m = parse_manifest(self, manifest_json)?;
        let slug = slugify(&m.name);
        if !valid_slug(&slug) {
            return Err(bad("`name` does not make a valid slug"));
        }
        let key = crypto::generate_key()?;
        let app_id = self.db.ids().ulid()?;
        let key_id = self.db.ids().ulid()?;
        let principal_id = self.db.ids().ulid()?;
        let client_id = format!("Iv1.{}", self.rand_hex(10)?);
        let client_secret = format!("cs_{}", self.rand_hex(20)?);
        let webhook_secret = self.rand_hex(20)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        if !tx
            .scan(&APPS, &Scan::scope("", 1).key(Key::K1(slug.clone())))
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("app {slug}")));
        }
        let index_scope = format!("owner:{}", owner.key());
        let owned = tx.count(&INDEX, &index_scope, None).await?;
        if owned >= MAX_APPS_PER_OWNER {
            return Err(ForgeError::too_large(
                "apps_per_owner",
                MAX_APPS_PER_OWNER,
                owned + 1,
            ));
        }
        let app = App {
            id: app_id.clone(),
            slug: slug.clone(),
            name: m.name,
            description: m.description,
            owner: owner.name.clone(),
            principal_id: principal_id.clone(),
            client_id: client_id.clone(),
            homepage_url: m.url,
            callback_urls: m.callback_urls,
            setup_url: m.setup_url,
            request_oauth_on_install: m.request_oauth_on_install,
            webhook_url: m.hook_url,
            webhook_active: m.hook_active,
            permissions: catalog::to_proto(&m.permissions),
            events: m.events,
            public: m.public,
            private_keys: Vec::new(),
            installation_count: 0,
            created_at: tsm(now),
            updated_at: tsm(now),
        };
        let rec = AppRec {
            app: Some(app.clone()),
            owner_key: owner.key(),
            client_secret_hash: sha256_hex(&client_secret),
            webhook_secret: webhook_secret.clone(),
        };
        tx.insert(
            &APPS,
            &Row::new("", app_id.clone())
                .k1(slug.clone())
                .k2(client_id)
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        tx.insert(&INDEX, &Row::new(index_scope, app_id.clone()).k1(slug))
            .await?;
        tx.insert(
            &INDEX,
            &Row::new("principal", principal_id.clone()).k1(app_id.clone()),
        )
        .await?;
        let krec = KeyRec {
            key: Some(keysum(&key_id, &key.fingerprint, now)),
            public_key: key.public_key.clone(),
        };
        tx.insert(
            &KEYS,
            &Row::new(app_id.clone(), key_id.clone())
                .k1(key.fingerprint.clone())
                .msg(&krec),
        )
        .await?;
        tx.audit(
            AuditDraft::new("app.create", owner.org_id())
                .target_id(&app_id)
                .after(&json(&app)),
        )
        .await?;
        let mut view = app_view(&mut tx, &rec).await?;
        tx.commit().await?;
        view.owner = owner.name.clone();
        self.forge
            .hooks
            .directory
            .observe(&bee_proto::v1::Principal {
                id: principal_id,
                kind: PrincipalKind::Service as i32,
                display: format!("{}[app]", view.slug),
                ..Default::default()
            });
        Ok(NewApp {
            app: view,
            client_secret,
            webhook_secret,
            private_key_pem: key.pem,
            private_key_id: key_id,
        })
    }

    /// Reads an app by slug, id or client id (secrets are never part of the result).
    pub async fn get_app(&self, cx: &Cx, by: &str) -> Result<App> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_app(&mut tx, by).await?;
        let mut app = app_view(&mut tx, &rec).await?;
        drop(tx);
        self.fill_owner(cx, &mut app, &rec.owner_key).await;
        Ok(app)
    }

    /// The owner key (`org:<id>` / `user:<id>`) of an app.
    pub async fn app_owner_key(&self, cx: &Cx, by: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(load_app(&mut tx, by).await?.0.owner_key)
    }

    /// Applies a patch. Raising permissions leaves installations on their old grant and marks
    /// them `permissions_update_pending` until [`Apps::accept_installation_permissions`];
    /// lowering applies at once. Webhook settings and events are pushed to every
    /// installation's hooks.
    pub async fn update_app(&self, cx: &Cx, by: &str, patch: AppPatch) -> Result<App> {
        for (f, u) in [
            ("homepage_url", &patch.homepage_url),
            ("setup_url", &patch.setup_url),
            ("webhook_url", &patch.webhook_url),
        ] {
            if let Some(u) = u
                && !u.is_empty()
            {
                check_url(f, u, "bad_url")?;
            }
        }
        if let Some(c) = &patch.callback_urls {
            check_urls("callback_urls", c, "bad_url")?;
        }
        let events = match &patch.events {
            Some(e) => Some(check_events(self, e, "unknown_event_kind")?),
            None => None,
        };
        if let Some(n) = &patch.name
            && (n.trim().is_empty() || n.chars().count() > 34)
        {
            return Err(ForgeError::invalid(
                "invalid_name",
                "name is 1-34 characters",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (mut rec, row) = load_app(&mut tx, by).await?;
        let mut app = rec.app.clone().unwrap_or_default();
        let before = json(&app);
        let hooks_before = (
            app.webhook_url.clone(),
            app.webhook_active,
            app.events.clone(),
            rec.webhook_secret.clone(),
        );
        if let Some(v) = patch.name {
            app.name = v.trim().to_string();
        }
        if let Some(v) = patch.description {
            app.description = v;
        }
        if let Some(v) = patch.homepage_url {
            app.homepage_url = v;
        }
        if let Some(v) = patch.callback_urls {
            app.callback_urls = v;
        }
        if let Some(v) = patch.setup_url {
            app.setup_url = v;
        }
        if let Some(v) = patch.request_oauth_on_install {
            app.request_oauth_on_install = v;
        }
        if let Some(v) = patch.webhook_url {
            app.webhook_url = v;
        }
        if let Some(v) = patch.webhook_active {
            app.webhook_active = v;
        }
        if let Some(v) = patch.webhook_secret {
            if v.len() < 8 {
                return Err(ForgeError::invalid(
                    "weak_secret",
                    "the webhook secret needs at least 8 characters",
                ));
            }
            rec.webhook_secret = v;
        }
        if let Some(v) = events {
            app.events = v;
        }
        if let Some(v) = patch.public {
            app.public = v;
        }
        if app.webhook_active && !app.events.is_empty() && app.webhook_url.is_empty() {
            return Err(ForgeError::invalid(
                "bad_url",
                "an active webhook with events needs a webhook URL",
            ));
        }
        let mut permission_change = false;
        if let Some(p) = patch.permissions {
            let new = catalog::with_metadata(p);
            let old = catalog::from_proto(&app.permissions, "bad_manifest")?;
            permission_change = new != old;
            app.permissions = catalog::to_proto(&new);
            if permission_change {
                super::installs::reconcile_permissions(&mut tx, &app.id, &new, now).await?;
            }
        }
        app.updated_at = tsm(now);
        let hooks_changed = hooks_before
            != (
                app.webhook_url.clone(),
                app.webhook_active,
                app.events.clone(),
                rec.webhook_secret.clone(),
            );
        rec.app = Some(app.clone());
        tx.update(&APPS, &row.clone().msg(&rec)).await?;
        let owner_org = rec.owner_key.strip_prefix("org:").unwrap_or("").to_string();
        tx.audit(
            AuditDraft::new("app.update", &owner_org)
                .target_id(&app.id)
                .before(&before)
                .after(&json(&app)),
        )
        .await?;
        let mut view = app_view(&mut tx, &rec).await?;
        tx.commit().await?;
        let _ = permission_change;
        if hooks_changed {
            self.resync_app_hooks(cx, &app.id).await?;
        }
        self.fill_owner(cx, &mut view, &rec.owner_key).await;
        Ok(view)
    }

    /// Deletes an app with every installation (emitting `app.uninstalled` in each target
    /// org), tracked token, webhook and OAuth grant it has; every token is revoked.
    pub async fn delete_app(&self, cx: &Cx, by: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_app(&mut tx, by).await?;
        let app = rec.app.clone().unwrap_or_default();
        let installs =
            scan_all::<super::InstallRec>(&mut tx, &INSTALLS, "", Some(Key::K1(app.id.clone())))
                .await?;
        let mut revoke = Vec::new();
        let mut hooks = Vec::new();
        for (inst, _) in installs {
            let removed = super::installs::drop_installation(&mut tx, &inst, "app deleted").await?;
            revoke.extend(removed.token_ids);
            hooks.extend(removed.hooks);
        }
        revoke.extend(super::oauth::drop_client(&mut tx, &app.client_id).await?);
        for (k, _) in scan_all::<KeyRec>(&mut tx, &KEYS, &app.id, None).await? {
            if let Some(k) = k.key {
                tx.delete(&KEYS, &app.id, &k.id).await?;
            }
        }
        tx.delete(&INDEX, &format!("owner:{}", rec.owner_key), &app.id)
            .await?;
        tx.delete(&INDEX, "principal", &app.principal_id).await?;
        tx.delete(&APPS, "", &app.id).await?;
        let owner_org = rec.owner_key.strip_prefix("org:").unwrap_or("").to_string();
        tx.audit(
            AuditDraft::new("app.delete", &owner_org)
                .target_id(&app.id)
                .before(&json(&app)),
        )
        .await?;
        super::installs::revoke_before_commit(self.cfg.revoker.as_ref(), &revoke).await?;
        tx.commit().await?;
        self.delete_hooks(cx, hooks).await;
        Ok(())
    }

    /// Apps of an owner, by id, paged.
    pub async fn list_apps(
        &self,
        cx: &Cx,
        owner: &Account,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<App>, String)> {
        let scope = format!("owner:{}", owner.key());
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) = page_rows(&mut tx, &INDEX, &scope, None, after, limit).await?;
        let mut out = Vec::new();
        for r in rows {
            if let Some((rec, _)) = tx.get_msg::<AppRec>(&APPS, "", &r.id).await? {
                let mut v = app_view(&mut tx, &rec).await?;
                v.owner = owner.name.clone();
                out.push(v);
            }
        }
        Ok((out, next))
    }

    /// Generates another private key for an app; the PEM is returned once.
    pub async fn create_private_key(&self, cx: &Cx, by: &str) -> Result<(AppPrivateKey, String)> {
        let key = crypto::generate_key()?;
        let key_id = self.db.ids().ulid()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (rec, _) = load_app(&mut tx, by).await?;
        let app_id = rec.app.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        let have = tx.count(&KEYS, &app_id, None).await?;
        if have >= MAX_KEYS_PER_APP {
            return Err(ForgeError::too_large(
                "keys_per_app",
                MAX_KEYS_PER_APP,
                have + 1,
            ));
        }
        let summary = keysum(&key_id, &key.fingerprint, now);
        tx.insert(
            &KEYS,
            &Row::new(app_id.clone(), key_id.clone())
                .k1(key.fingerprint.clone())
                .msg(&KeyRec {
                    key: Some(summary.clone()),
                    public_key: key.public_key,
                }),
        )
        .await?;
        let owner_org = rec.owner_key.strip_prefix("org:").unwrap_or("").to_string();
        tx.audit(
            AuditDraft::new("app.key_create", &owner_org)
                .target_id(&app_id)
                .after(&json(&summary)),
        )
        .await?;
        tx.commit().await?;
        Ok((summary, key.pem))
    }

    /// Deletes a private key; the last key cannot be deleted (`INVALID_STATE`).
    pub async fn delete_private_key(&self, cx: &Cx, by: &str, key_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_app(&mut tx, by).await?;
        let app_id = rec.app.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        let (_k, _) = get_or_404::<KeyRec>(&mut tx, &KEYS, &app_id, key_id, "private key").await?;
        if tx.count(&KEYS, &app_id, None).await? <= 1 {
            return Err(ForgeError::invalid_state(
                "the last private key of an app cannot be deleted",
            ));
        }
        tx.delete(&KEYS, &app_id, key_id).await?;
        let owner_org = rec.owner_key.strip_prefix("org:").unwrap_or("").to_string();
        tx.audit(
            AuditDraft::new("app.key_delete", &owner_org)
                .target_id(&app_id)
                .before(&format!("{{\"key_id\":\"{key_id}\"}}")),
        )
        .await?;
        tx.commit().await
    }

    /// Authenticates an app JWT (`iss` = client id or app id, `kid` = key id, `exp` within ten
    /// minutes of `iat`) against the app's keys and returns the app.
    pub async fn authenticate_app_jwt(&self, jwt: &str) -> Result<App> {
        let parts = crypto::parse_jwt(jwt)?;
        crypto::check_jwt_times(&parts, self.now_secs())?;
        let cx = Cx::system();
        let denied =
            |m: &str| ForgeError::new(ErrorReason::Unauthenticated, format!("app JWT: {m}"));
        let mut tx = self.db.begin(SURFACE, &cx).await?;
        let (rec, _) = load_app(&mut tx, &parts.iss)
            .await
            .map_err(|_| denied("unknown app"))?;
        let app_id = rec.app.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        let keys = scan_all::<KeyRec>(&mut tx, &KEYS, &app_id, None).await?;
        let mut used = None;
        for (k, row) in keys {
            let id = k.key.as_ref().map(|s| s.id.as_str()).unwrap_or("");
            if parts.kid.as_deref().is_some_and(|kid| kid != id) {
                continue;
            }
            if crypto::verify_jwt_signature(&parts, &k.public_key) {
                used = Some((k, row));
                break;
            }
        }
        let Some((mut key, row)) = used else {
            return Err(denied(
                "signature does not verify against any key of the app",
            ));
        };
        let view = app_view(&mut tx, &rec).await?;
        drop(tx);
        // Record use at most once a minute, best effort.
        let now = self.now_ms();
        let last = key
            .key
            .as_ref()
            .and_then(|k| k.last_used_at.as_ref())
            .map(crate::ids::ts_ms)
            .unwrap_or(0);
        if now.saturating_sub(last) > 60_000 {
            if let Some(s) = key.key.as_mut() {
                s.last_used_at = tsm(now);
            }
            if let Ok(mut tx) = self.db.begin(SURFACE, &cx).await
                && tx.update(&KEYS, &row.msg(&key)).await.is_ok()
            {
                let _ = tx.commit().await;
            }
        }
        Ok(view)
    }

    /// Webhook rows owned by installations of an app (internal; used by deletions).
    pub(crate) async fn delete_hooks(&self, cx: &Cx, hooks: Vec<super::HookLink>) {
        for h in hooks {
            let owner = if h.repo_id.is_empty() {
                crate::events::webhooks::HookOwner::org(&h.org_id)
            } else {
                crate::events::webhooks::HookOwner::repo(&h.repo_id, &h.org_id)
            };
            let _ = self.forge.webhooks.delete(cx, &owner, &h.hook_id).await;
        }
    }
}
