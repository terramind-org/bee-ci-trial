//! The OAuth provider: OAuth apps, the authorization-code flow with PKCE, the device flow,
//! refresh-token rotation, user-to-server tokens and grants.
//!
//! # Flows
//!
//! - **Code.** [`Apps::create_authorization`] records the signed-in user's consent and
//!   returns a single-use code (10 minutes). [`Apps::exchange_code`] checks the client
//!   (secret, or the PKCE `S256` verifier in constant time), the exact `redirect_uri` and the
//!   code; a second use of a code revokes every token issued from it.
//! - **Device.** [`Apps::start_device_flow`] returns a device code and an `XXXX-XXXX` user
//!   code (15 minutes, polling interval 5 s). The user approves with
//!   [`Apps::create_authorization`] (`user_code`); [`Apps::poll_device_flow`] answers
//!   `PENDING`, `SLOW_DOWN` (polled before the interval elapsed; the interval grows by 5 s),
//!   `APPROVED` with tokens, `DENIED` or `EXPIRED`.
//! - **Refresh.** Refresh tokens rotate on use; presenting a rotated one revokes the whole
//!   token family.
//!
//! # Tokens
//!
//! Access tokens are Biscuits. For an OAuth app the token acts as the user, limited to the
//! actions of the granted scopes. For an app's client id the token is a *user-to-server*
//! token: principal = the app principal, `on_behalf_of` = the user (chain `[app, user]`),
//! repos = exactly the repos covered by the app's active installations on orgs the user is a
//! member of, actions = those of the installations' permissions. Authorization additionally
//! needs both principals to hold roles on the repo, so the result is the intersection of the
//! user's access and the app's installations.

use std::collections::BTreeSet;

use bee_auth::{MintRequest, Principal as AuthPrincipal, PrincipalRef, Scope};
use bee_policy::Action;
use bee_proto::v1::{
    App, ErrorReason, OAuthApp, OAuthClientKind, OAuthDeviceFlowStatus, OAuthGrant, OAuthTokenSet,
    PrincipalKind,
};

use super::catalog::{self, Level};
use super::crypto;
use super::registry::{check_url, load_app, page_rows};
use super::{
    AppRec, Apps, CODES, CodeRec, DEVICE, DeviceRec, GRANTS, INSTALLS, InstallRec, OAUTH_APPS,
    OAuthAppRec, OTOKENS, REFRESH, RefreshRec, SURFACE, TokenRec, tsm,
};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::enterprise::common::{ct_eq, json, scan_all, sha256_hex};
use crate::error::{ForgeError, Result};
use crate::store::{Key, Row, Scan, Tx};

/// Authorization code lifetime.
pub const CODE_TTL_SECS: u64 = 600;
/// Device flow lifetime.
pub const DEVICE_TTL_SECS: u64 = 900;
/// Minimum device polling interval.
pub const DEVICE_INTERVAL_SECS: u32 = 5;
/// Most OAuth apps one account owns.
pub const MAX_OAUTH_APPS_PER_OWNER: usize = 100;

const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

/// A new OAuth app.
#[derive(Clone, Debug, Default)]
pub struct NewOAuthApp {
    /// Name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Homepage.
    pub homepage_url: String,
    /// Registered callback URLs.
    pub callback_urls: Vec<String>,
    /// Whether the device flow may be used.
    pub device_flow_enabled: bool,
}

/// A partial OAuth app update.
#[derive(Clone, Debug, Default)]
pub struct OAuthAppPatch {
    /// Name.
    pub name: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Homepage.
    pub homepage_url: Option<String>,
    /// Callback URLs (replaces the list).
    pub callback_urls: Option<Vec<String>>,
    /// Device flow switch.
    pub device_flow_enabled: Option<bool>,
    /// Generate a new client secret.
    pub reset_client_secret: bool,
}

/// A consent decision (code flow with `client_id`, device flow with `user_code`).
#[derive(Clone, Debug, Default)]
pub struct AuthorizationRequest {
    /// Code flow: the client.
    pub client_id: String,
    /// Device flow: the code the user typed.
    pub user_code: String,
    /// Code flow: must match a registered callback exactly.
    pub redirect_uri: String,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Echoed back in the redirect.
    pub state: String,
    /// PKCE challenge.
    pub code_challenge: String,
    /// PKCE method (`S256`).
    pub code_challenge_method: String,
    /// The user declined.
    pub deny: bool,
    /// OpenID Connect: the client's `nonce`, echoed in the id_token.
    pub nonce: String,
}

/// What a consent decision produced.
#[derive(Clone, Debug, Default)]
pub struct AuthorizationOutcome {
    /// The code (code flow, approved).
    pub code: String,
    /// `redirect_uri` with `code` and `state`, or `error=access_denied`.
    pub redirect_url: String,
    /// Code expiry in ms.
    pub code_expires_ms: u64,
}

/// A started device flow.
#[derive(Clone, Debug, Default)]
pub struct DeviceFlowStart {
    /// The secret code the client polls with.
    pub device_code: String,
    /// The `XXXX-XXXX` code the user types.
    pub user_code: String,
    /// Where the user goes.
    pub verification_uri: String,
    /// The same with the code filled in.
    pub verification_uri_complete: String,
    /// Expiry in ms.
    pub expires_ms: u64,
    /// Minimum polling interval.
    pub interval_secs: u32,
}

/// One poll answer.
#[derive(Clone, Debug)]
pub struct DevicePoll {
    /// Status.
    pub status: OAuthDeviceFlowStatus,
    /// Tokens, once approved.
    pub tokens: Option<OAuthTokenSet>,
    /// The polling interval now in force.
    pub interval_secs: u32,
}

/// What to revoke.
#[derive(Clone, Debug)]
pub enum RevokeTarget {
    /// A whole grant and every token under it.
    Client(String),
    /// One token.
    Token(String),
}

/// A resolved OAuth client: an OAuth app or an app.
struct Client {
    kind: OAuthClientKind,
    client_id: String,
    name: String,
    owner_key: String,
    secret_hash: String,
    callbacks: Vec<String>,
    device_enabled: bool,
    app: Option<App>,
}

/// Who a token is for and what it may do.
struct UserScope {
    repos: Option<BTreeSet<String>>,
    actions: BTreeSet<Action>,
    scopes: Vec<String>,
}

struct Minted {
    token: bee_auth::Token,
    exp_ms: u64,
    refresh: String,
    refresh_exp_ms: u64,
}

fn invalid_client() -> ForgeError {
    ForgeError::new(
        ErrorReason::Unauthenticated,
        "unknown or mismatching OAuth client",
    )
    .with_subcode("invalid_client")
}

fn invalid_grant(m: &str) -> ForgeError {
    ForgeError::invalid("invalid_grant", m)
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn with_query(base: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = base.to_string();
    let mut sep = if base.contains('?') { '&' } else { '?' };
    for (k, v) in pairs {
        if v.is_empty() {
            continue;
        }
        out.push(sep);
        sep = '&';
        out.push_str(k);
        out.push('=');
        out.push_str(&pct(v));
    }
    out
}

/// Normalizes a typed user code to `XXXX-XXXX`.
fn norm_user_code(s: &str) -> String {
    let c: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if c.len() == 8 {
        format!("{}-{}", &c[..4], &c[4..])
    } else {
        c
    }
}

async fn client_of(tx: &mut Tx, client_id: &str) -> Result<Client> {
    if client_id.starts_with("Iv1.") {
        let rows = tx
            .scan_msgs::<AppRec>(
                &super::APPS,
                &Scan::scope("", 1).key(Key::K2(client_id.to_string())),
            )
            .await?;
        let Some((rec, _)) = rows.into_iter().next() else {
            return Err(invalid_client());
        };
        let app = rec.app.clone().unwrap_or_default();
        return Ok(Client {
            kind: OAuthClientKind::OauthClientKindApp,
            client_id: app.client_id.clone(),
            name: app.name.clone(),
            owner_key: rec.owner_key.clone(),
            secret_hash: rec.client_secret_hash.clone(),
            callbacks: app.callback_urls.clone(),
            device_enabled: true,
            app: Some(app),
        });
    }
    let rows = tx
        .scan_msgs::<OAuthAppRec>(
            &OAUTH_APPS,
            &Scan::scope("", 1).key(Key::K1(client_id.to_string())),
        )
        .await?;
    let Some((rec, _)) = rows.into_iter().next() else {
        return Err(invalid_client());
    };
    let app = rec.app.clone().unwrap_or_default();
    Ok(Client {
        kind: OAuthClientKind::OauthClientKindOauthApp,
        client_id: app.client_id.clone(),
        name: app.name.clone(),
        owner_key: rec.owner_key.clone(),
        secret_hash: rec.client_secret_hash.clone(),
        callbacks: app.callback_urls.clone(),
        device_enabled: app.device_flow_enabled,
        app: None,
    })
}

fn check_secret(client: &Client, secret: &str) -> Result<()> {
    if !secret.is_empty() && ct_eq(&sha256_hex(secret), &client.secret_hash) {
        Ok(())
    } else {
        Err(invalid_client())
    }
}

/// Removes every grant, token and refresh token of a client in `tx`; returns the token ids
/// to revoke.
pub(crate) async fn drop_client(tx: &mut Tx, client_id: &str) -> Result<Vec<String>> {
    let grants = tx
        .scan(
            &GRANTS,
            &Scan {
                key: Some(Key::K1(client_id.to_string())),
                limit: crate::store::MAX_SCAN,
                ..Scan::default()
            },
        )
        .await?;
    let mut ids = Vec::new();
    for g in grants {
        ids.extend(drop_grant(tx, &g.scope, client_id).await?);
    }
    Ok(ids)
}

/// Removes one user's grant to a client with its tokens and refresh tokens.
async fn drop_grant(tx: &mut Tx, principal: &str, client_id: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for (_, row) in scan_all::<TokenRec>(
        tx,
        &OTOKENS,
        principal,
        Some(Key::K1(client_id.to_string())),
    )
    .await?
    {
        ids.push(row.id.clone());
        tx.delete(&OTOKENS, principal, &row.id).await?;
    }
    let pair = format!("{principal}/{client_id}");
    for (_, row) in scan_all::<RefreshRec>(tx, &REFRESH, "", Some(Key::K2(pair))).await? {
        tx.delete(&REFRESH, "", &row.id).await?;
    }
    tx.delete(&GRANTS, principal, client_id).await?;
    Ok(ids)
}

fn user_cx(cx: &Cx, user_id: &str) -> Cx {
    let mut c = cx.clone();
    c.actor_id = user_id.to_string();
    c.actor_kind = PrincipalKind::Human;
    c.chain = vec![user_id.to_string()];
    c
}

impl Apps {
    // ---------- OAuth apps ----------

    fn oauth_view(rec: &OAuthAppRec) -> OAuthApp {
        rec.app.clone().unwrap_or_default()
    }

    /// Registers an OAuth app; the client secret is returned once.
    pub async fn create_oauth_app(
        &self,
        cx: &Cx,
        owner: &super::Account,
        new: NewOAuthApp,
    ) -> Result<(OAuthApp, String)> {
        if new.name.trim().is_empty() || new.name.chars().count() > 100 {
            return Err(ForgeError::invalid(
                "invalid_name",
                "name is 1-100 characters",
            ));
        }
        if !new.homepage_url.is_empty() {
            check_url("homepage_url", &new.homepage_url, "bad_url")?;
        }
        if new.callback_urls.len() > 10 {
            return Err(ForgeError::invalid("bad_url", "at most 10 callback URLs"));
        }
        for u in &new.callback_urls {
            check_url("callback_urls", u, "bad_url")?;
        }
        let id = self.db.ids().ulid()?;
        let client_id = format!("Ov1.{}", self.rand_hex(10)?);
        let secret = format!("ocs_{}", self.rand_hex(20)?);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let owned = tx
            .count(&OAUTH_APPS, "", Some(&Key::K2(owner.key())))
            .await? as usize;
        if owned >= MAX_OAUTH_APPS_PER_OWNER {
            return Err(ForgeError::too_large(
                "oauth_apps_per_owner",
                MAX_OAUTH_APPS_PER_OWNER as u64,
                owned as u64 + 1,
            ));
        }
        let app = OAuthApp {
            id: id.clone(),
            owner: owner.name.clone(),
            name: new.name.trim().to_string(),
            description: new.description,
            client_id: client_id.clone(),
            homepage_url: new.homepage_url,
            callback_urls: new.callback_urls,
            device_flow_enabled: new.device_flow_enabled,
            logo_url: String::new(),
            created_at: tsm(now),
            updated_at: tsm(now),
        };
        let rec = OAuthAppRec {
            app: Some(app.clone()),
            owner_key: owner.key(),
            client_secret_hash: sha256_hex(&secret),
        };
        tx.insert(
            &OAUTH_APPS,
            &Row::new("", id.clone())
                .k1(client_id)
                .k2(owner.key())
                .msg(&rec),
        )
        .await?;
        tx.audit(
            AuditDraft::new("oauth.app_create", owner.org_id())
                .target_id(&id)
                .after(&json(&app)),
        )
        .await?;
        tx.commit().await?;
        Ok((app, secret))
    }

    async fn load_oauth_app(tx: &mut Tx, by: &str) -> Result<(OAuthAppRec, Row)> {
        if by.is_empty() {
            return Err(ForgeError::missing("oauth_app"));
        }
        if let Some((rec, row)) = tx.get_msg::<OAuthAppRec>(&OAUTH_APPS, "", by).await? {
            return Ok((rec, row));
        }
        let rows = tx
            .scan(
                &OAUTH_APPS,
                &Scan::scope("", 1).key(Key::K1(by.to_string())),
            )
            .await?;
        match rows.into_iter().next() {
            Some(row) => Ok((row.decode()?, row)),
            None => Err(ForgeError::not_found(format!("OAuth app {by}"))),
        }
    }

    /// Reads an OAuth app by client id or id.
    pub async fn get_oauth_app(&self, cx: &Cx, by: &str) -> Result<OAuthApp> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load_oauth_app(&mut tx, by).await?;
        drop(tx);
        let mut app = Self::oauth_view(&rec);
        app.owner = self.account_name(cx, &rec.owner_key).await;
        Ok(app)
    }

    /// The owner key of an OAuth app.
    pub async fn oauth_app_owner_key(&self, cx: &Cx, by: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::load_oauth_app(&mut tx, by).await?.0.owner_key)
    }

    /// Changes an OAuth app; returns the new client secret when it was reset.
    pub async fn update_oauth_app(
        &self,
        cx: &Cx,
        by: &str,
        patch: OAuthAppPatch,
    ) -> Result<(OAuthApp, Option<String>)> {
        if let Some(u) = &patch.homepage_url
            && !u.is_empty()
        {
            check_url("homepage_url", u, "bad_url")?;
        }
        if let Some(c) = &patch.callback_urls {
            if c.len() > 10 {
                return Err(ForgeError::invalid("bad_url", "at most 10 callback URLs"));
            }
            for u in c {
                check_url("callback_urls", u, "bad_url")?;
            }
        }
        if let Some(n) = &patch.name
            && (n.trim().is_empty() || n.chars().count() > 100)
        {
            return Err(ForgeError::invalid(
                "invalid_name",
                "name is 1-100 characters",
            ));
        }
        let new_secret = if patch.reset_client_secret {
            Some(format!("ocs_{}", self.rand_hex(20)?))
        } else {
            None
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (mut rec, row) = Self::load_oauth_app(&mut tx, by).await?;
        let mut app = Self::oauth_view(&rec);
        let before = json(&app);
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
        if let Some(v) = patch.device_flow_enabled {
            app.device_flow_enabled = v;
        }
        app.updated_at = tsm(now);
        if let Some(s) = &new_secret {
            rec.client_secret_hash = sha256_hex(s);
        }
        rec.app = Some(app.clone());
        tx.update(&OAUTH_APPS, &row.clone().msg(&rec)).await?;
        let org = rec.owner_key.strip_prefix("org:").unwrap_or("").to_string();
        tx.audit(
            AuditDraft::new("oauth.app_update", &org)
                .target_id(&app.id)
                .before(&before)
                .after(&serde_json::json!({
                    "app": serde_json::from_str::<serde_json::Value>(&json(&app)).unwrap_or_default(),
                    "secret_reset": new_secret.is_some(),
                }).to_string()),
        )
        .await?;
        tx.commit().await?;
        app.owner = self.account_name(cx, &rec.owner_key).await;
        Ok((app, new_secret))
    }

    /// Deletes an OAuth app and revokes every token it holds.
    pub async fn delete_oauth_app(&self, cx: &Cx, by: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::load_oauth_app(&mut tx, by).await?;
        let app = Self::oauth_view(&rec);
        let ids = drop_client(&mut tx, &app.client_id).await?;
        tx.delete(&OAUTH_APPS, "", &app.id).await?;
        let org = rec.owner_key.strip_prefix("org:").unwrap_or("").to_string();
        tx.audit(
            AuditDraft::new("oauth.app_delete", &org)
                .target_id(&app.id)
                .before(&json(&app)),
        )
        .await?;
        tx.commit().await?;
        for id in &ids {
            let _ = self.cfg.revoker.revoke(id).await;
        }
        Ok(())
    }

    /// OAuth apps of an owner, by id, paged.
    pub async fn list_oauth_apps(
        &self,
        cx: &Cx,
        owner: &super::Account,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<OAuthApp>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) = page_rows(
            &mut tx,
            &OAUTH_APPS,
            "",
            Some(Key::K2(owner.key())),
            after,
            limit,
        )
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let rec: OAuthAppRec = r.decode()?;
            let mut a = Self::oauth_view(&rec);
            a.owner = owner.name.clone();
            out.push(a);
        }
        Ok((out, next))
    }

    // ---------- helpers ----------

    async fn member_org_ids(&self, cx: &Cx, user: &str) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        let mut after = String::new();
        for _ in 0..100 {
            let (orgs, next) = self.forge.orgs.orgs_of(cx, user, &after, 500).await?;
            out.extend(orgs.into_iter().map(|o| o.id));
            if next.is_empty() {
                break;
            }
            after = next;
        }
        Ok(out)
    }

    async fn check_sso(&self, cx: &Cx, user: &str) -> Result<()> {
        if let Some(g) = &self.cfg.sso {
            let orgs: Vec<String> = self.member_org_ids(cx, user).await?.into_iter().collect();
            g.check_sso(cx, user, &orgs).await?;
        }
        Ok(())
    }

    /// The scope of a token for `user` on `client`.
    async fn user_scope(
        &self,
        cx: &Cx,
        client: &Client,
        user: &str,
        scopes: &[String],
    ) -> Result<UserScope> {
        let Some(app) = &client.app else {
            return Ok(UserScope {
                repos: None,
                actions: catalog::scope_actions(scopes),
                scopes: scopes.to_vec(),
            });
        };
        let member_orgs = self.member_org_ids(cx, user).await?;
        let installs = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            scan_all::<InstallRec>(&mut tx, &INSTALLS, "", Some(Key::K1(app.id.clone()))).await?
        };
        let mut repos = BTreeSet::new();
        let mut perms = catalog::Permissions::new();
        for (rec, _) in installs {
            let Some(inst) = &rec.inst else { continue };
            if inst.suspended
                || inst.target_kind != bee_proto::v1::AppInstallationTargetKind::Org as i32
                || !member_orgs.contains(&rec.target_id)
            {
                continue;
            }
            repos.extend(self.covered_repo_ids(cx, &rec).await?);
            for (r, l) in catalog::from_proto(&inst.permissions, "bad_permission")? {
                let e = perms.entry(r).or_insert(l);
                *e = (*e).max(l);
            }
        }
        Ok(UserScope {
            repos: Some(repos),
            actions: catalog::actions_of(&perms),
            scopes: perms
                .iter()
                .map(|(r, l): (&String, &Level)| format!("{r}:{}", l.as_str()))
                .collect(),
        })
    }

    async fn auth_principal(
        &self,
        user: &str,
    ) -> Result<(AuthPrincipal, bee_policy::PrincipalKind)> {
        let p = self.forge.hooks.directory.get(user).await;
        let kind = p
            .as_ref()
            .and_then(|p| PrincipalKind::try_from(p.kind).ok())
            .unwrap_or(PrincipalKind::Human);
        let display = p.map(|p| p.display).unwrap_or_default();
        match kind {
            PrincipalKind::Service => Ok((
                AuthPrincipal::service(user, display),
                bee_policy::PrincipalKind::Service,
            )),
            PrincipalKind::Agent => Err(ForgeError::invalid(
                "unsupported_principal",
                "agents do not authorize OAuth clients; their owners do",
            )),
            _ => Ok((
                AuthPrincipal::human(user, display),
                bee_policy::PrincipalKind::Human,
            )),
        }
    }

    /// Mints the access token and the refresh token text.
    async fn mint(&self, client: &Client, user: &str, scope: &UserScope) -> Result<Minted> {
        let now = self.now_secs();
        let exp = now + self.cfg.access_token_ttl_secs;
        let mut sc = Scope::new()
            .actions(scope.actions.iter().copied())
            .expires_at(exp);
        if let Some(r) = &scope.repos {
            sc = sc.repos(r.iter())?;
        }
        let (user_principal, user_kind) = self.auth_principal(user).await?;
        let req = match &client.app {
            None => MintRequest {
                principal: user_principal,
                on_behalf_of: Vec::new(),
                scope: sc,
            },
            Some(app) => MintRequest {
                principal: AuthPrincipal::service(
                    app.principal_id.clone(),
                    format!("{}[app]", app.slug),
                ),
                on_behalf_of: vec![PrincipalRef::new(user, user_kind)],
                scope: sc,
            },
        };
        let token = self.cfg.keys.mint(&req, now)?;
        Ok(Minted {
            token,
            exp_ms: exp * 1000,
            refresh: format!("rt_{}", self.rand_hex(32)?),
            refresh_exp_ms: (now + self.cfg.refresh_token_ttl_secs) * 1000,
        })
    }

    /// Records a minted set (token row, refresh row, grant use) and builds the response.
    #[allow(clippy::too_many_arguments)]
    async fn record_tokens(
        &self,
        tx: &mut Tx,
        client: &Client,
        user: &str,
        scope: &UserScope,
        m: &Minted,
        family: &str,
        public_client: bool,
    ) -> Result<OAuthTokenSet> {
        let now = tx.now_ms();
        tx.insert(
            &OTOKENS,
            &Row::new(user, m.token.id.0.clone())
                .k1(client.client_id.clone())
                .k2(family)
                .n1(m.exp_ms as i64)
                .msg(&TokenRec {
                    expires_ms: m.exp_ms,
                }),
        )
        .await?;
        tx.insert(
            &REFRESH,
            &Row::new("", sha256_hex(&m.refresh))
                .k1(family)
                .k2(format!("{user}/{}", client.client_id))
                .n1(m.refresh_exp_ms as i64)
                .msg(&RefreshRec {
                    client_id: client.client_id.clone(),
                    principal_id: user.to_string(),
                    family: family.to_string(),
                    scopes: scope.scopes.clone(),
                    rotated: false,
                    public_client,
                }),
        )
        .await?;
        if let Some((mut g, row)) = tx
            .get_msg::<OAuthGrant>(&GRANTS, user, &client.client_id)
            .await?
        {
            g.last_used_at = tsm(now);
            tx.update(&GRANTS, &row.msg(&g)).await?;
        }
        Ok(OAuthTokenSet {
            access_token: m.token.text.clone(),
            token_type: "bearer".into(),
            token_id: m.token.id.0.clone(),
            scopes: scope.scopes.clone(),
            expires_at: tsm(m.exp_ms),
            refresh_token: m.refresh.clone(),
            refresh_token_expires_at: tsm(m.refresh_exp_ms),
            client_kind: client.kind as i32,
        })
    }

    async fn put_grant(
        &self,
        tx: &mut Tx,
        client: &Client,
        user: &str,
        scopes: &[String],
    ) -> Result<()> {
        let now = tx.now_ms();
        let existing = tx
            .get_msg::<OAuthGrant>(&GRANTS, user, &client.client_id)
            .await?;
        let (granted_at, last_used) = match &existing {
            Some((g, _)) => (g.granted_at, g.last_used_at),
            None => (tsm(now), None),
        };
        let g = OAuthGrant {
            client_id: client.client_id.clone(),
            client_kind: client.kind as i32,
            client_name: client.name.clone(),
            owner: String::new(),
            scopes: scopes.to_vec(),
            granted_at,
            last_used_at: last_used,
        };
        let row = Row::new(user, client.client_id.clone()).k1(client.client_id.clone());
        match existing {
            Some((_, old)) => {
                tx.update(&GRANTS, &row.version(old.version).msg(&g))
                    .await?;
            }
            None => tx.insert(&GRANTS, &row.msg(&g)).await?,
        }
        Ok(())
    }

    // ---------- consent ----------

    /// Records the signed-in user's decision. With `client_id` (code flow) it returns a
    /// single-use code and the redirect; with `user_code` (device flow) it approves or
    /// denies the pending device flow. `SSO_REQUIRED` comes from the [`super::SsoGate`].
    pub async fn create_authorization(
        &self,
        cx: &Cx,
        req: AuthorizationRequest,
    ) -> Result<AuthorizationOutcome> {
        if !req.user_code.is_empty() {
            return self.answer_device(cx, &req).await;
        }
        if req.client_id.is_empty() {
            return Err(ForgeError::missing("client_id"));
        }
        let client = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            client_of(&mut tx, &req.client_id).await?
        };
        if req.redirect_uri.is_empty() || !client.callbacks.contains(&req.redirect_uri) {
            return Err(ForgeError::invalid(
                "redirect_uri_mismatch",
                "redirect_uri must exactly match a registered callback URL",
            ));
        }
        if !req.code_challenge.is_empty() {
            let ok_chars = req
                .code_challenge
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if req.code_challenge_method != "S256" || req.code_challenge.len() != 43 || !ok_chars {
                return Err(ForgeError::invalid(
                    "bad_code_challenge",
                    "PKCE needs code_challenge_method S256 and a 43 character base64url challenge",
                ));
            }
        }
        let scopes = match client.kind {
            OAuthClientKind::OauthClientKindApp => Vec::new(),
            _ => catalog::check_scopes(&req.scopes)?,
        };
        if req.deny {
            return Ok(AuthorizationOutcome {
                code: String::new(),
                redirect_url: with_query(
                    &req.redirect_uri,
                    &[("error", "access_denied"), ("state", &req.state)],
                ),
                code_expires_ms: 0,
            });
        }
        let user = cx.actor_id.clone();
        self.check_sso(cx, &user).await?;
        let code = format!("sc_{}", self.rand_hex(32)?);
        let family = self.db.ids().ulid()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let exp = now + CODE_TTL_SECS * 1000;
        self.put_grant(&mut tx, &client, &user, &scopes).await?;
        tx.insert(
            &CODES,
            &Row::new("", sha256_hex(&code))
                .n1(exp as i64)
                .msg(&CodeRec {
                    client_id: client.client_id.clone(),
                    principal_id: user.clone(),
                    redirect_uri: req.redirect_uri.clone(),
                    scopes,
                    challenge: req.code_challenge.clone(),
                    used: false,
                    family,
                    nonce: req.nonce.clone(),
                }),
        )
        .await?;
        tx.audit(
            AuditDraft::new("oauth.authorize", "")
                .target_id(&client.client_id)
                .after(&serde_json::json!({ "client_id": client.client_id, "client_name": client.name }).to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(AuthorizationOutcome {
            redirect_url: with_query(&req.redirect_uri, &[("code", &code), ("state", &req.state)]),
            code,
            code_expires_ms: exp,
        })
    }

    async fn answer_device(
        &self,
        cx: &Cx,
        req: &AuthorizationRequest,
    ) -> Result<AuthorizationOutcome> {
        let code = norm_user_code(&req.user_code);
        if !req.deny {
            self.check_sso(cx, &cx.actor_id).await?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let rows = tx
            .scan(&DEVICE, &Scan::scope("", 1).key(Key::K1(code.clone())))
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Err(ForgeError::not_found(format!("device code {code}")));
        };
        let mut rec: DeviceRec = row.decode()?;
        if row.n1.unwrap_or(0) as u64 <= now {
            return Err(
                ForgeError::invalid_state("the device code expired").with_subcode("expired")
            );
        }
        if rec.status != OAuthDeviceFlowStatus::OauthDeviceFlowStatusPending as i32 {
            return Err(ForgeError::invalid_state(
                "the device code was already answered",
            ));
        }
        let client = client_of(&mut tx, &rec.client_id).await?;
        if req.deny {
            rec.status = OAuthDeviceFlowStatus::OauthDeviceFlowStatusDenied as i32;
        } else {
            rec.status = OAuthDeviceFlowStatus::OauthDeviceFlowStatusApproved as i32;
            rec.principal_id = cx.actor_id.clone();
            let scopes = rec.scopes.clone();
            self.put_grant(&mut tx, &client, &cx.actor_id, &scopes)
                .await?;
        }
        tx.update(&DEVICE, &row.clone().msg(&rec)).await?;
        tx.audit(
            AuditDraft::new(
                if req.deny {
                    "oauth.device_deny"
                } else {
                    "oauth.device_approve"
                },
                "",
            )
            .target_id(&client.client_id),
        )
        .await?;
        tx.commit().await?;
        Ok(AuthorizationOutcome::default())
    }

    // ---------- code exchange ----------

    /// The OpenID Connect `nonce` an authorization code was issued with (empty when none or
    /// when the code is unknown), read before the code is exchanged.
    pub async fn code_nonce(&self, cx: &Cx, code: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get_msg::<CodeRec>(&CODES, "", &sha256_hex(code))
            .await?
            .map(|(r, _)| r.nonce)
            .unwrap_or_default())
    }

    /// Exchanges an authorization code for tokens (see the module docs).
    pub async fn exchange_code(
        &self,
        cx: &Cx,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<OAuthTokenSet> {
        let hash = sha256_hex(code);
        let (client, rec, row) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let client = client_of(&mut tx, client_id).await?;
            let Some((rec, row)) = tx.get_msg::<CodeRec>(&CODES, "", &hash).await? else {
                return Err(invalid_grant("unknown authorization code"));
            };
            (client, rec, row)
        };
        if rec.client_id != client.client_id {
            return Err(invalid_grant("the code was issued to another client"));
        }
        let now = self.now_ms();
        if row.n1.unwrap_or(0) as u64 <= now {
            return Err(invalid_grant("the authorization code expired"));
        }
        if !client_secret.is_empty() {
            check_secret(&client, client_secret)?;
        }
        if rec.used {
            self.revoke_family(cx, &rec.principal_id, &rec.family)
                .await?;
            return Err(invalid_grant(
                "the code was already used; tokens issued from it were revoked",
            ));
        }
        if rec.redirect_uri != redirect_uri {
            return Err(invalid_grant(
                "redirect_uri differs from the authorization request",
            ));
        }
        let public_client = client_secret.is_empty();
        if !rec.challenge.is_empty() {
            if !crypto::pkce_matches(&rec.challenge, code_verifier) {
                // A wrong verifier burns the code.
                let mut tx = self.db.begin(SURFACE, cx).await?;
                tx.delete(&CODES, "", &hash).await?;
                tx.commit().await?;
                return Err(invalid_grant(
                    "code_verifier does not match the code_challenge",
                ));
            }
        } else if public_client {
            return Err(invalid_client());
        }
        let user = rec.principal_id.clone();
        let ucx = user_cx(cx, &user);
        let scope = self.user_scope(&ucx, &client, &user, &rec.scopes).await?;
        let minted = self.mint(&client, &user, &scope).await?;
        let mut tx = self.db.begin(SURFACE, &ucx).await?;
        let Some((mut cur, cur_row)) = tx.get_msg::<CodeRec>(&CODES, "", &hash).await? else {
            return Err(invalid_grant("unknown authorization code"));
        };
        if cur.used {
            drop(tx);
            self.revoke_family(cx, &user, &rec.family).await?;
            return Err(invalid_grant("the code was already used"));
        }
        cur.used = true;
        match tx.update(&CODES, &cur_row.clone().msg(&cur)).await {
            Ok(_) => {}
            Err(e) if e.reason == ErrorReason::VersionMismatch => {
                return Err(invalid_grant("the code was already used"));
            }
            Err(e) => return Err(e),
        }
        let set = self
            .record_tokens(
                &mut tx,
                &client,
                &user,
                &scope,
                &minted,
                &rec.family,
                public_client,
            )
            .await?;
        tx.audit(
            AuditDraft::new("oauth.token_issue", "")
                .target_id(&set.token_id)
                .after(
                    &serde_json::json!({ "client_id": client.client_id, "flow": "code" })
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        Ok(set)
    }

    /// Revokes every token and refresh token of a family.
    async fn revoke_family(&self, cx: &Cx, user: &str, family: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut ids = Vec::new();
        for (_, row) in
            scan_all::<TokenRec>(&mut tx, &OTOKENS, user, Some(Key::K2(family.to_string()))).await?
        {
            ids.push(row.id.clone());
            tx.delete(&OTOKENS, user, &row.id).await?;
        }
        for (_, row) in
            scan_all::<RefreshRec>(&mut tx, &REFRESH, "", Some(Key::K1(family.to_string()))).await?
        {
            tx.delete(&REFRESH, "", &row.id).await?;
        }
        tx.audit(
            AuditDraft::new("oauth.family_revoke", "")
                .target_id(family)
                .after(
                    &serde_json::json!({ "principal_id": user, "tokens": ids.len() }).to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        for id in &ids {
            let _ = self.cfg.revoker.revoke(id).await;
        }
        Ok(())
    }

    /// Exchanges a refresh token for a new token set; the presented token is spent. Presenting
    /// a spent token revokes the whole family (`TOKEN_REVOKED`, subcode
    /// `refresh_token_reused`).
    pub async fn refresh_token(
        &self,
        cx: &Cx,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<OAuthTokenSet> {
        let hash = sha256_hex(refresh_token);
        let (client, rec, row) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let client = client_of(&mut tx, client_id).await?;
            let Some((rec, row)) = tx.get_msg::<RefreshRec>(&REFRESH, "", &hash).await? else {
                return Err(invalid_grant("unknown refresh token"));
            };
            (client, rec, row)
        };
        if rec.client_id != client.client_id {
            return Err(invalid_grant("the refresh token belongs to another client"));
        }
        if !rec.public_client || !client_secret.is_empty() {
            check_secret(&client, client_secret)?;
        }
        let reused = || {
            ForgeError::new(
                ErrorReason::TokenRevoked,
                "refresh token reuse detected; every token of the grant was revoked",
            )
            .with_subcode("refresh_token_reused")
        };
        if rec.rotated {
            self.revoke_family(cx, &rec.principal_id, &rec.family)
                .await?;
            return Err(reused());
        }
        if row.n1.unwrap_or(0) as u64 <= self.now_ms() {
            return Err(
                ForgeError::new(ErrorReason::TokenExpired, "the refresh token expired")
                    .with_subcode("invalid_grant"),
            );
        }
        let user = rec.principal_id.clone();
        let ucx = user_cx(cx, &user);
        {
            let mut tx = self.db.begin(SURFACE, &ucx).await?;
            if tx.get(&GRANTS, &user, &client.client_id).await?.is_none() {
                return Err(invalid_grant("the grant was revoked"));
            }
        }
        let scope = self.user_scope(&ucx, &client, &user, &rec.scopes).await?;
        let minted = self.mint(&client, &user, &scope).await?;
        let mut tx = self.db.begin(SURFACE, &ucx).await?;
        let mut spent = rec.clone();
        spent.rotated = true;
        match tx.update(&REFRESH, &row.clone().msg(&spent)).await {
            Ok(_) => {}
            Err(e) if e.reason == ErrorReason::VersionMismatch => {
                drop(tx);
                self.revoke_family(cx, &user, &rec.family).await?;
                return Err(reused());
            }
            Err(e) => return Err(e),
        }
        let set = self
            .record_tokens(
                &mut tx,
                &client,
                &user,
                &scope,
                &minted,
                &rec.family,
                rec.public_client,
            )
            .await?;
        tx.audit(
            AuditDraft::new("oauth.token_refresh", "")
                .target_id(&set.token_id)
                .after(&serde_json::json!({ "client_id": client.client_id }).to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(set)
    }

    // ---------- device flow ----------

    /// Starts a device flow for a client.
    pub async fn start_device_flow(
        &self,
        cx: &Cx,
        client_id: &str,
        scopes: &[String],
    ) -> Result<DeviceFlowStart> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let client = client_of(&mut tx, client_id).await?;
        if !client.device_enabled {
            return Err(
                ForgeError::invalid_state("the client does not allow the device flow")
                    .with_subcode("device_flow_disabled"),
            );
        }
        let scopes = match client.kind {
            OAuthClientKind::OauthClientKindApp => Vec::new(),
            _ => catalog::check_scopes(scopes)?,
        };
        let device_code = format!("dc_{}", self.rand_hex(32)?);
        let now = tx.now_ms();
        let exp = now + DEVICE_TTL_SECS * 1000;
        let mut user_code = String::new();
        for _ in 0..8 {
            let raw = crate::ids::unhex(&self.rand_hex(8)?).unwrap_or_default();
            let chars: String = raw
                .iter()
                .map(|b| USER_CODE_ALPHABET[*b as usize % USER_CODE_ALPHABET.len()] as char)
                .collect();
            let candidate = format!("{}-{}", &chars[..4], &chars[4..]);
            if tx
                .scan(&DEVICE, &Scan::scope("", 1).key(Key::K1(candidate.clone())))
                .await?
                .is_empty()
            {
                user_code = candidate;
                break;
            }
        }
        if user_code.is_empty() {
            return Err(ForgeError::unavailable(
                "could not allocate a user code; retry",
            ));
        }
        tx.insert(
            &DEVICE,
            &Row::new("", sha256_hex(&device_code))
                .k1(user_code.clone())
                .n1(exp as i64)
                .msg(&DeviceRec {
                    client_id: client.client_id.clone(),
                    scopes,
                    status: OAuthDeviceFlowStatus::OauthDeviceFlowStatusPending as i32,
                    principal_id: String::new(),
                    last_poll_ms: 0,
                    interval_secs: DEVICE_INTERVAL_SECS,
                }),
        )
        .await?;
        tx.commit().await?;
        Ok(DeviceFlowStart {
            verification_uri_complete: with_query(
                &self.cfg.verification_uri,
                &[("user_code", &user_code)],
            ),
            verification_uri: self.cfg.verification_uri.clone(),
            device_code,
            user_code,
            expires_ms: exp,
            interval_secs: DEVICE_INTERVAL_SECS,
        })
    }

    /// Polls a device flow (see the module docs for the answers).
    pub async fn poll_device_flow(
        &self,
        cx: &Cx,
        client_id: &str,
        device_code: &str,
    ) -> Result<DevicePoll> {
        let hash = sha256_hex(device_code);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let client = client_of(&mut tx, client_id).await?;
        let Some((mut rec, row)) = tx.get_msg::<DeviceRec>(&DEVICE, "", &hash).await? else {
            return Err(invalid_grant("unknown device code"));
        };
        if rec.client_id != client.client_id {
            return Err(invalid_grant("the device code belongs to another client"));
        }
        let reply = |status, tokens, interval_secs| DevicePoll {
            status,
            tokens,
            interval_secs,
        };
        if row.n1.unwrap_or(0) as u64 <= now {
            return Ok(reply(
                OAuthDeviceFlowStatus::OauthDeviceFlowStatusExpired,
                None,
                rec.interval_secs,
            ));
        }
        let status = OAuthDeviceFlowStatus::try_from(rec.status)
            .unwrap_or(OAuthDeviceFlowStatus::OauthDeviceFlowStatusPending);
        match status {
            OAuthDeviceFlowStatus::OauthDeviceFlowStatusDenied => {
                Ok(reply(status, None, rec.interval_secs))
            }
            OAuthDeviceFlowStatus::OauthDeviceFlowStatusApproved => {
                let user = rec.principal_id.clone();
                drop(tx);
                let ucx = user_cx(cx, &user);
                let scope = self.user_scope(&ucx, &client, &user, &rec.scopes).await?;
                let minted = self.mint(&client, &user, &scope).await?;
                let family = self.db.ids().ulid()?;
                let mut tx = self.db.begin(SURFACE, &ucx).await?;
                if !tx.delete(&DEVICE, "", &hash).await? {
                    return Err(invalid_grant("the device code was already used"));
                }
                let set = self
                    .record_tokens(&mut tx, &client, &user, &scope, &minted, &family, true)
                    .await?;
                tx.audit(
                    AuditDraft::new("oauth.token_issue", "")
                        .target_id(&set.token_id)
                        .after(
                            &serde_json::json!({ "client_id": client.client_id, "flow": "device" })
                                .to_string(),
                        ),
                )
                .await?;
                tx.commit().await?;
                Ok(reply(status, Some(set), rec.interval_secs))
            }
            _ => {
                let too_fast = rec.last_poll_ms > 0
                    && now.saturating_sub(rec.last_poll_ms) < u64::from(rec.interval_secs) * 1000;
                rec.last_poll_ms = now;
                if too_fast {
                    rec.interval_secs += DEVICE_INTERVAL_SECS;
                }
                match tx.update(&DEVICE, &row.clone().msg(&rec)).await {
                    Ok(_) => tx.commit().await?,
                    Err(e) if e.reason == ErrorReason::VersionMismatch => {
                        return Ok(reply(
                            OAuthDeviceFlowStatus::OauthDeviceFlowStatusSlowDown,
                            None,
                            rec.interval_secs + DEVICE_INTERVAL_SECS,
                        ));
                    }
                    Err(e) => return Err(e),
                }
                Ok(reply(
                    if too_fast {
                        OAuthDeviceFlowStatus::OauthDeviceFlowStatusSlowDown
                    } else {
                        OAuthDeviceFlowStatus::OauthDeviceFlowStatusPending
                    },
                    None,
                    rec.interval_secs,
                ))
            }
        }
    }

    // ---------- grants ----------

    /// Revokes a grant (and every token under it) or a single token of `principal_id`.
    pub async fn revoke_oauth_grant(
        &self,
        cx: &Cx,
        principal_id: &str,
        target: RevokeTarget,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let ids = match &target {
            RevokeTarget::Client(client_id) => {
                if tx.get(&GRANTS, principal_id, client_id).await?.is_none() {
                    return Err(ForgeError::not_found(format!("grant to {client_id}")));
                }
                drop_grant(&mut tx, principal_id, client_id).await?
            }
            RevokeTarget::Token(token_id) => {
                let Some((_, row)) = tx
                    .get_msg::<TokenRec>(&OTOKENS, principal_id, token_id)
                    .await?
                else {
                    return Err(ForgeError::not_found(format!("token {token_id}")));
                };
                tx.delete(&OTOKENS, principal_id, token_id).await?;
                if let Some(family) = &row.k2 {
                    for (_, r) in
                        scan_all::<RefreshRec>(&mut tx, &REFRESH, "", Some(Key::K1(family.clone())))
                            .await?
                    {
                        tx.delete(&REFRESH, "", &r.id).await?;
                    }
                }
                vec![token_id.clone()]
            }
        };
        tx.audit(
            AuditDraft::new("oauth.revoke", "")
                .target_id(match &target {
                    RevokeTarget::Client(c) => c,
                    RevokeTarget::Token(t) => t,
                })
                .after(
                    &serde_json::json!({ "principal_id": principal_id, "tokens": ids.len() })
                        .to_string(),
                ),
        )
        .await?;
        tx.commit().await?;
        for id in &ids {
            let _ = self.cfg.revoker.revoke(id).await;
        }
        Ok(())
    }

    /// The OAuth clients `principal_id` has authorized, by client id, paged.
    pub async fn list_oauth_grants(
        &self,
        cx: &Cx,
        principal_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<OAuthGrant>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) = page_rows(&mut tx, &GRANTS, principal_id, None, after, limit).await?;
        let mut out = Vec::new();
        let mut owners = Vec::new();
        for r in rows {
            let mut g: OAuthGrant = r.decode()?;
            let Ok(client) = client_of(&mut tx, &g.client_id).await else {
                continue;
            };
            g.client_name = client.name;
            owners.push(client.owner_key);
            out.push(g);
        }
        drop(tx);
        for (g, key) in out.iter_mut().zip(owners) {
            g.owner = self.account_name(cx, &key).await;
        }
        Ok((out, next))
    }

    /// The app behind an app client id (used by the service for SSO and audit context).
    pub async fn app_by_client_id(&self, cx: &Cx, client_id: &str) -> Result<App> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(load_app(&mut tx, client_id)
            .await?
            .0
            .app
            .unwrap_or_default())
    }
}
