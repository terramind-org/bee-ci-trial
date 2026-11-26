//! SAML single sign-on for an org or an enterprise: configuration, test sign-in, assertion
//! consumption, identities, sessions and enforcement (`EnterpriseService` SAML RPCs).
//!
//! # Flow
//!
//! 1. An admin stores the IdP's metadata (or entity id, SSO URL and signing certificates) with
//!    [`Sso::update_config`]. Any change to the IdP settings clears the test mark.
//! 2. [`Sso::test_config`] validates the metadata (problems such as an expiring certificate
//!    are listed) and returns a sign-in URL. Completing that sign-in through
//!    [`Sso::consume_response`] marks the configuration tested and links the admin's identity.
//! 3. Enforcement ([`Sso::update_config`] with `enforced`) is refused (`INVALID_STATE`,
//!    `sso_untested`) until a test sign-in succeeded. Once on, [`super::network::Network::admit`]
//!    fails every request of an org member without a live session with `SSO_REQUIRED`.
//! 4. Login: the SP redirects to the IdP with an AuthnRequest whose id is the pending-request
//!    id (also the RelayState); the IdP posts a signed response; [`Sso::consume_response`]
//!    validates it ([`super::saml`]), refuses a replayed assertion id, finds the principal
//!    (linked identity, the authenticated principal that started the sign-in, or the SCIM
//!    identity whose user name or email equals the NameID), stores the groups the IdP sent
//!    (team sync) and opens a session lasting `session_lifetime_seconds` (capped by the IdP's
//!    own session end). IdP-initiated responses (no RelayState) are accepted for existing
//!    identities.
//!
//! Metadata URLs are fetched by a worker outside the request path; it hands the XML to
//! [`Sso::update_config`] as `idp_metadata_xml`.

use std::collections::BTreeSet;
use std::sync::Arc;

use bee_proto::v1::{SamlIdentity, SamlSsoConfig, SubjectKind};
use prost::Message;

use super::SURFACE;
use super::accounts::{self, kind, put_setting, setting, update_flags};
use super::common::{
    self, ResolvedScope, ScopeKind, audit_enterprise, principal_of, scan_all, scan_page,
};
use super::saml::{self, Certificate, Expect};
use super::scim::SCIM_USERS;
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::ts;
use crate::store::{Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, TableDef, Tx};

macro_rules! table {
    ($name:ident, $table:literal, $uk1:expr, [$($idx:ident),*], $doc:literal) => {
        #[doc = $doc]
        pub static $name: TableDef = TableDef {
            name: $table,
            surface: SURFACE,
            unique_k1: $uk1,
            unique_k2: false,
            indexes: &[$(Index::$idx),*],
        };
    };
}

table!(
    SAML_REQUESTS,
    "forge_saml_requests",
    false,
    [N1],
    "Pending sign-in requests, scoped by scope key; id the request id, `n1` expiry."
);
table!(
    SAML_IDENTITIES,
    "forge_saml_identities",
    true,
    [ScopeK2],
    "Linked identities, scoped by scope key; id the principal id, `k1` NameID (unique), `k2` lower-case email."
);
table!(
    SAML_SESSIONS,
    "forge_saml_sessions",
    false,
    [N1],
    "SSO sessions, scoped by scope key; id the principal id, `n1` expiry."
);
table!(
    SAML_ASSERTIONS,
    "forge_saml_assertions",
    false,
    [N1],
    "Consumed assertion ids (replay cache), scoped by scope key; `n1` expiry."
);

/// Migration creating the SSO tables.
pub static M_SSO: Migration = Migration {
    module: "enterprise",
    version: 5,
    name: "create_saml_sso",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&SAML_REQUESTS),
        MigrationOp::CreateTable(&SAML_IDENTITIES),
        MigrationOp::CreateTable(&SAML_SESSIONS),
        MigrationOp::CreateTable(&SAML_ASSERTIONS),
    ],
};

/// Default SSO session lifetime.
pub const DEFAULT_SESSION_SECS: u32 = 8 * 3600;
/// Longest SSO session lifetime.
pub const MAX_SESSION_SECS: u32 = 30 * 24 * 3600;
/// How long a started sign-in may take.
pub const REQUEST_TTL_SECS: u64 = 600;
/// Attribute names that carry the IdP's groups.
pub const GROUP_ATTRIBUTES: &[&str] = &[
    "groups",
    "memberOf",
    "http://schemas.xmlsoap.org/claims/Group",
    "http://schemas.microsoft.com/ws/2008/06/identity/claims/groups",
];
const EMAIL_ATTRIBUTES: &[&str] = &[
    "email",
    "mail",
    "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress",
];

#[derive(Clone, PartialEq, Message)]
struct Pending {
    #[prost(string, tag = "1")]
    purpose: String,
    #[prost(string, tag = "2")]
    principal_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct Session {
    #[prost(string, tag = "1")]
    session_index: String,
    #[prost(string, tag = "2")]
    assertion_id: String,
}

/// A partial SSO configuration update.
#[derive(Clone, Debug, Default)]
pub struct SsoPatch {
    /// Members may link and use SSO.
    pub enabled: Option<bool>,
    /// Members must have a live session.
    pub enforced: Option<bool>,
    /// Where the IdP's metadata is published (fetched by a worker).
    pub idp_metadata_url: Option<String>,
    /// IdP metadata XML (replaces entity id, SSO URL and certificates).
    pub idp_metadata_xml: Option<String>,
    /// IdP entity id.
    pub idp_entity_id: Option<String>,
    /// IdP SSO URL.
    pub idp_sso_url: Option<String>,
    /// IdP signing certificate(s), PEM.
    pub idp_certificate_pem: Option<String>,
    /// Expected signature method, informational.
    pub signature_method: Option<String>,
    /// Expected digest method, informational.
    pub digest_method: Option<String>,
    /// Session lifetime in seconds.
    pub session_lifetime_seconds: Option<u32>,
}

/// The outcome of consuming a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    /// The signed-in principal.
    pub principal_id: String,
    /// When the session ends (ms).
    pub session_expires_ms: u64,
    /// The identity was linked by this sign-in.
    pub linked: bool,
    /// The sign-in was the configuration test.
    pub test: bool,
    /// Groups the IdP sent.
    pub groups: Vec<String>,
}

/// Result of [`Sso::test_config`].
#[derive(Clone, Debug, Default)]
pub struct TestResult {
    /// The metadata parses and the certificates are usable.
    pub metadata_ok: bool,
    /// Findings, for example `certificate expires in 6 days`.
    pub problems: Vec<String>,
    /// Open in a browser to complete a test sign-in (empty when the metadata is not ok).
    pub test_sign_in_url: String,
}

/// Parses every certificate in a PEM bundle.
pub fn parse_certs(pem: &str) -> Result<Vec<Certificate>> {
    let mut out = Vec::new();
    for block in pem.split("-----END CERTIFICATE-----") {
        if block.trim().is_empty() {
            continue;
        }
        out.push(saml::parse_certificate(block)?);
    }
    if out.is_empty() {
        return Err(ForgeError::invalid(
            "bad_certificate",
            "no certificate found",
        ));
    }
    Ok(out)
}

/// Whether the principal has a live SSO session under `scope_key`.
pub async fn session_live(tx: &mut Tx, scope_key: &str, principal_id: &str) -> Result<bool> {
    let now = tx.now_ms() as i64;
    Ok(tx
        .get(&SAML_SESSIONS, scope_key, principal_id)
        .await?
        .and_then(|r| r.n1)
        .is_some_and(|n| n > now))
}

/// SAML SSO over the enterprise surface.
pub struct Sso {
    db: Db,
    forge: Arc<Forge>,
    base_url: String,
}

impl Sso {
    /// SSO over the forge's store; `base_url` is the public URL of this deployment (it forms
    /// the SP entity ids and ACS URLs).
    pub fn new(forge: Arc<Forge>, base_url: &str) -> Sso {
        Sso {
            db: forge.db.clone(),
            forge,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// The SP entity id of a scope.
    pub fn sp_entity_id(&self, scope: &ResolvedScope) -> String {
        let seg = match scope.kind {
            ScopeKind::Org => "orgs",
            ScopeKind::Enterprise => "enterprises",
        };
        format!("{}/saml/{seg}/{}", self.base_url, scope.name)
    }

    /// The assertion consumer service URL of a scope.
    pub fn acs_url(&self, scope: &ResolvedScope) -> String {
        format!("{}/acs", self.sp_entity_id(scope))
    }

    /// SP metadata XML for a scope.
    pub fn sp_metadata(&self, scope: &ResolvedScope) -> String {
        saml::sp_metadata(&self.sp_entity_id(scope), &self.acs_url(scope))
    }

    fn fill(&self, scope: &ResolvedScope, mut c: SamlSsoConfig) -> SamlSsoConfig {
        c.scope = Some(scope.to_proto());
        c.sp_entity_id = self.sp_entity_id(scope);
        c.sp_acs_url = self.acs_url(scope);
        if c.session_lifetime_seconds == 0 {
            c.session_lifetime_seconds = DEFAULT_SESSION_SECS;
        }
        c.idp_certificate_expires_at = parse_certs(&c.idp_certificate_pem)
            .ok()
            .and_then(|cs| cs.iter().map(|c| c.not_after).min())
            .map(|s| ts(s * 1000));
        c
    }

    async fn load(tx: &mut Tx, key: &str) -> Result<SamlSsoConfig> {
        Ok(setting::<SamlSsoConfig>(tx, key, kind::SAML)
            .await?
            .map(|(c, _)| c)
            .unwrap_or_default())
    }

    /// The configuration of a scope.
    pub async fn config(&self, cx: &Cx, scope: &ResolvedScope) -> Result<SamlSsoConfig> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(self.fill(scope, Self::load(&mut tx, &scope.key()).await?))
    }

    async fn record<B: serde::Serialize, A: serde::Serialize>(
        tx: &mut Tx,
        scope: &ResolvedScope,
        action: &str,
        target: &str,
        before: &B,
        after: &A,
    ) -> Result<()> {
        let draft = AuditDraft::new(action, &scope.audit_org())
            .target(SubjectKind::Unspecified, target)
            .before(&common::json(before))
            .after(&common::json(after));
        match scope.kind {
            ScopeKind::Org => tx.audit(draft).await,
            ScopeKind::Enterprise => audit_enterprise(tx, &scope.id, draft).await,
        }
    }

    /// Changes the configuration (see the module docs for the rules).
    pub async fn update_config(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        p: SsoPatch,
    ) -> Result<SamlSsoConfig> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let before = Self::load(&mut tx, &key).await?;
        let mut c = before.clone();
        let mut idp_changed = false;
        if let Some(xml) = &p.idp_metadata_xml {
            let md = saml::parse_idp_metadata(xml)?;
            c.idp_metadata_xml = xml.clone();
            c.idp_entity_id = md.entity_id;
            c.idp_sso_url = md.sso_url;
            c.idp_certificate_pem = md.certificates.join("");
            idp_changed = true;
        }
        if let Some(u) = p.idp_metadata_url {
            c.idp_metadata_url = u;
        }
        if let Some(v) = p.idp_entity_id {
            c.idp_entity_id = v;
            idp_changed = true;
        }
        if let Some(v) = p.idp_sso_url {
            if !v.starts_with("https://") && !v.starts_with("http://") {
                return Err(ForgeError::invalid(
                    "bad_url",
                    "the SSO URL must be http(s)",
                ));
            }
            c.idp_sso_url = v;
            idp_changed = true;
        }
        if let Some(v) = p.idp_certificate_pem {
            parse_certs(&v)?;
            c.idp_certificate_pem = v;
            idp_changed = true;
        }
        if let Some(v) = p.signature_method {
            c.signature_method = v;
        }
        if let Some(v) = p.digest_method {
            c.digest_method = v;
        }
        if let Some(v) = p.session_lifetime_seconds {
            if v == 0 || v > MAX_SESSION_SECS {
                return Err(ForgeError::invalid(
                    "invalid_setting",
                    format!("session_lifetime_seconds is between 1 and {MAX_SESSION_SECS}"),
                ));
            }
            c.session_lifetime_seconds = v;
        }
        if idp_changed {
            c.last_tested_at = None;
        }
        if let Some(v) = p.enabled {
            c.enabled = v;
        }
        if let Some(v) = p.enforced {
            c.enforced = v;
        }
        if c.enabled {
            if c.idp_entity_id.is_empty() || c.idp_sso_url.is_empty() {
                return Err(ForgeError::invalid_state(
                    "SSO needs the IdP entity id and SSO URL (or its metadata)",
                )
                .with_subcode("sso_incomplete"));
            }
            let certs = parse_certs(&c.idp_certificate_pem)?;
            let now = tx.now_ms() / 1000;
            if certs.iter().all(|x| x.not_after <= now) {
                return Err(
                    ForgeError::invalid_state("every IdP certificate has expired")
                        .with_subcode("sso_certificate_expired"),
                );
            }
        }
        if c.enforced && !c.enabled {
            return Err(ForgeError::invalid_state(
                "enforcement needs SSO to be enabled",
            ));
        }
        if c.enforced && c.last_tested_at.is_none() {
            return Err(
                ForgeError::invalid_state("complete a test sign-in before enforcing SSO")
                    .with_subcode("sso_untested"),
            );
        }
        c.updated_at = Some(ts(tx.now_ms()));
        put_setting(&mut tx, &key, kind::SAML, &c).await?;
        let enforced = c.enforced;
        update_flags(&mut tx, &key, |f| f.sso_enforced = enforced).await?;
        Self::record(&mut tx, scope, "enterprise.sso_update", &key, &before, &c).await?;
        tx.commit().await?;
        Ok(self.fill(scope, c))
    }

    /// Validates metadata (the stored one, or `override_xml`) and starts a test sign-in for
    /// the calling principal.
    pub async fn test_config(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        override_xml: &str,
    ) -> Result<TestResult> {
        let c = self.config(cx, scope).await?;
        let mut result = TestResult::default();
        let (entity, sso_url, pem) = if override_xml.is_empty() {
            (
                c.idp_entity_id.clone(),
                c.idp_sso_url.clone(),
                c.idp_certificate_pem.clone(),
            )
        } else {
            match saml::parse_idp_metadata(override_xml) {
                Ok(m) => (m.entity_id, m.sso_url, m.certificates.join("")),
                Err(e) => {
                    result.problems.push(e.message);
                    return Ok(result);
                }
            }
        };
        if entity.is_empty() {
            result.problems.push("the IdP entity id is not set".into());
        }
        if sso_url.is_empty() {
            result.problems.push("the IdP SSO URL is not set".into());
        }
        let now = self.forge.db.now_ms() / 1000;
        match parse_certs(&pem) {
            Ok(certs) => {
                for cert in &certs {
                    if cert.not_after <= now {
                        result.problems.push("a certificate has expired".into());
                    } else if cert.not_after - now < 30 * 86_400 {
                        result.problems.push(format!(
                            "certificate expires in {} days",
                            (cert.not_after - now).div_ceil(86_400)
                        ));
                    }
                    if cert.not_before > now {
                        result
                            .problems
                            .push("a certificate is not valid yet".into());
                    }
                }
                if certs.iter().all(|c| c.not_after <= now) {
                    return Ok(result);
                }
            }
            Err(e) => {
                result.problems.push(e.message);
                return Ok(result);
            }
        }
        if entity.is_empty() || sso_url.is_empty() {
            return Ok(result);
        }
        result.metadata_ok = true;
        let (url, _) = self
            .start_sign_in(cx, scope, "test", &cx.actor_id, &sso_url)
            .await?;
        result.test_sign_in_url = url;
        Ok(result)
    }

    /// Starts a sign-in: records the pending request and returns the IdP redirect URL and the
    /// request id. `purpose` is `test` or `login`; `principal_id` is the authenticated
    /// principal that will be linked, if any.
    pub async fn start_sign_in(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        purpose: &str,
        principal_id: &str,
        sso_url: &str,
    ) -> Result<(String, String)> {
        let sso_url = if sso_url.is_empty() {
            self.config(cx, scope).await?.idp_sso_url
        } else {
            sso_url.to_string()
        };
        if sso_url.is_empty() {
            return Err(ForgeError::invalid_state("SSO is not configured"));
        }
        let id = format!("_{}", self.forge.db.ids().random_hex(16)?);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let pending = Pending {
            purpose: purpose.into(),
            principal_id: principal_id.into(),
        };
        tx.insert(
            &SAML_REQUESTS,
            &Row::new(scope.key(), id.clone())
                .n1((now + REQUEST_TTL_SECS * 1000) as i64)
                .msg(&pending),
        )
        .await?;
        tx.commit().await?;
        let xml = saml::authn_request(
            &id,
            now / 1000,
            &sso_url,
            &self.sp_entity_id(scope),
            &self.acs_url(scope),
        );
        Ok((saml::redirect_url(&sso_url, &xml, &id), id))
    }

    /// Consumes an IdP response (see the module docs). `relay_state` is the request id the
    /// IdP echoes (empty for IdP-initiated sign-in).
    pub async fn consume_response(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        saml_response: &str,
        relay_state: &str,
    ) -> Result<SignIn> {
        let key = scope.key();
        let (cfg, pending) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let cfg = Self::load(&mut tx, &key).await?;
            let now = tx.now_ms() as i64;
            let pending = if relay_state.is_empty() {
                None
            } else {
                match tx
                    .get_msg::<Pending>(&SAML_REQUESTS, &key, relay_state)
                    .await?
                {
                    Some((p, row)) if row.n1.unwrap_or(0) > now => Some(p),
                    _ => {
                        return Err(ForgeError::permission_denied(
                            "the sign-in request is unknown or expired",
                        )
                        .with_subcode("bad_relay_state"));
                    }
                }
            };
            (cfg, pending)
        };
        if !cfg.enabled {
            return Err(ForgeError::invalid_state(
                "SSO is not enabled for this scope",
            ));
        }
        let expect = Expect {
            idp_entity_id: cfg.idp_entity_id.clone(),
            sp_entity_id: self.sp_entity_id(scope),
            acs_url: self.acs_url(scope),
            certs: parse_certs(&cfg.idp_certificate_pem)?,
            now: (self.forge.db.now_ms() / 1000) as i64,
            in_response_to: pending.as_ref().map(|_| relay_state.to_string()),
        };
        let a = saml::validate_response(saml_response, &expect)?;
        let is_test = pending.as_ref().is_some_and(|p| p.purpose == "test");
        let hint = pending
            .as_ref()
            .map(|p| p.principal_id.clone())
            .unwrap_or_default();
        let groups: Vec<String> = {
            let mut g: BTreeSet<String> = BTreeSet::new();
            for name in GROUP_ATTRIBUTES {
                g.extend(a.attributes.get(*name).into_iter().flatten().cloned());
            }
            g.into_iter().collect()
        };
        let email = EMAIL_ATTRIBUTES
            .iter()
            .find_map(|n| a.attributes.get(*n).and_then(|v| v.first()))
            .cloned()
            .unwrap_or_else(|| {
                if a.name_id.contains('@') {
                    a.name_id.clone()
                } else {
                    String::new()
                }
            });
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        // Replay protection: an assertion id is consumed once.
        if tx.get(&SAML_ASSERTIONS, &key, &a.id).await?.is_some() {
            return Err(
                ForgeError::permission_denied("the assertion was already used")
                    .with_subcode("assertion_replayed"),
            );
        }
        tx.insert(
            &SAML_ASSERTIONS,
            &Row::new(key.clone(), a.id.clone())
                .n1(a.not_on_or_after.saturating_mul(1000).max(0) + 600_000),
        )
        .await?;
        // Who is this?
        let existing = tx
            .scan_msgs::<SamlIdentity>(
                &SAML_IDENTITIES,
                &Scan::scope(key.clone(), 1).key(Key::K1(a.name_id.clone())),
            )
            .await?
            .into_iter()
            .next();
        let (principal_id, mut identity, row, linked) = match existing {
            Some((i, r)) => {
                let pid = i
                    .principal
                    .as_ref()
                    .map(|p| p.id.clone())
                    .unwrap_or_default();
                if !hint.is_empty() && hint != pid {
                    return Err(ForgeError::already_exists(
                        "this IdP identity is linked to another principal",
                    ));
                }
                (pid, i, Some(r), false)
            }
            None => {
                let mut pid = hint.clone();
                let mut scim = false;
                if pid.is_empty() {
                    let users =
                        scan_all::<bee_proto::v1::ScimIdentity>(&mut tx, &SCIM_USERS, &key, None)
                            .await;
                    let wanted = a.name_id.to_lowercase();
                    if let Some((u, _)) = users?.into_iter().find(|(u, _)| {
                        u.active
                            && (u.user_name.to_lowercase() == wanted
                                || (!email.is_empty()
                                    && u.emails.iter().any(|e| e.eq_ignore_ascii_case(&email))))
                    }) && let Some(p) = u.principal
                    {
                        pid = p.id;
                        scim = true;
                    }
                }
                if pid.is_empty() {
                    return Err(ForgeError::permission_denied(
                        "no principal is linked to this IdP identity; start the sign-in while signed in to link it",
                    )
                    .with_subcode("no_identity"));
                }
                let p = principal_of(&self.forge, &pid).await;
                let ident = SamlIdentity {
                    principal: Some(p),
                    name_id: a.name_id.clone(),
                    email: email.clone(),
                    groups: Vec::new(),
                    scim_provisioned: scim,
                    linked_at: Some(ts(now)),
                    last_sign_in_at: None,
                    session_expires_at: None,
                };
                (pid, ident, None, true)
            }
        };
        if let Some(s) = self_blocked(&mut tx, &principal_id).await? {
            return Err(s);
        }
        let old_groups = identity.groups.clone();
        identity.groups = groups.clone();
        if !email.is_empty() {
            identity.email = email.clone();
        }
        identity.last_sign_in_at = Some(ts(now));
        let lifetime = u64::from(if cfg.session_lifetime_seconds == 0 {
            DEFAULT_SESSION_SECS
        } else {
            cfg.session_lifetime_seconds
        });
        let mut expires = now + lifetime * 1000;
        if let Some(end) = a.session_not_on_or_after {
            expires = expires.min((end.max(0) as u64) * 1000);
        }
        if expires <= now {
            return Err(ForgeError::permission_denied(
                "the IdP session has already ended",
            ));
        }
        identity.session_expires_at = Some(ts(expires));
        let irow = Row::new(key.clone(), principal_id.clone())
            .k1(a.name_id.clone())
            .k2(identity.email.to_lowercase())
            .msg(&identity);
        match row {
            Some(r) => {
                tx.update(
                    &SAML_IDENTITIES,
                    &Row {
                        version: r.version,
                        ..irow
                    },
                )
                .await?;
            }
            None => tx.insert(&SAML_IDENTITIES, &irow).await?,
        }
        tx.put(
            &SAML_SESSIONS,
            &Row::new(key.clone(), principal_id.clone())
                .n1(expires as i64)
                .msg(&Session {
                    session_index: a.session_index.clone(),
                    assertion_id: a.id.clone(),
                }),
        )
        .await?;
        if pending.is_some() {
            tx.delete(&SAML_REQUESTS, &key, relay_state).await?;
        }
        if is_test {
            let mut c = Self::load(&mut tx, &key).await?;
            c.last_tested_at = Some(ts(now));
            put_setting(&mut tx, &key, kind::SAML, &c).await?;
        }
        let after = serde_json::json!({
            "principal_id": principal_id, "name_id": a.name_id, "linked": linked,
            "test": is_test, "groups": groups, "session_expires_ms": expires,
        });
        Self::record(
            &mut tx,
            scope,
            if is_test {
                "enterprise.sso_test_sign_in"
            } else {
                "enterprise.sso_sign_in"
            },
            &principal_id,
            &(),
            &after,
        )
        .await?;
        tx.commit().await?;
        let mut touched: BTreeSet<String> = old_groups.into_iter().collect();
        touched.extend(groups.iter().cloned());
        if !touched.is_empty() {
            super::sync::resync_groups(&self.forge, cx, "saml", &touched).await?;
        }
        Ok(SignIn {
            principal_id,
            session_expires_ms: expires,
            linked,
            test: is_test,
            groups,
        })
    }

    /// Lists linked identities, optionally filtered by NameID, email or display name.
    pub async fn identities(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        query: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<SamlIdentity>, String)> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let q = query.to_lowercase();
        let (rows, next) =
            scan_page::<SamlIdentity>(&mut tx, &SAML_IDENTITIES, &key, None, after, limit).await?;
        let now = tx.now_ms() as i64;
        let mut out = Vec::new();
        for (mut i, r) in rows {
            let display = i
                .principal
                .as_ref()
                .map(|p| p.display.to_lowercase())
                .unwrap_or_default();
            if !q.is_empty()
                && !i.name_id.to_lowercase().contains(&q)
                && !i.email.to_lowercase().contains(&q)
                && !display.contains(&q)
            {
                continue;
            }
            let live = tx
                .get(&SAML_SESSIONS, &key, &r.id)
                .await?
                .and_then(|s| s.n1)
                .filter(|n| *n > now);
            i.session_expires_at = live.map(|n| ts(n as u64));
            out.push(i);
        }
        Ok((out, next))
    }

    /// Ends a principal's session; with `unlink`, also removes the identity.
    pub async fn revoke_session(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        principal_id: &str,
        unlink: bool,
    ) -> Result<()> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let had_session = tx.delete(&SAML_SESSIONS, &key, principal_id).await?;
        let had_identity = if unlink {
            tx.delete(&SAML_IDENTITIES, &key, principal_id).await?
        } else {
            tx.get(&SAML_IDENTITIES, &key, principal_id)
                .await?
                .is_some()
        };
        if !had_session && !had_identity {
            return Err(ForgeError::not_found(format!(
                "SSO identity of {principal_id}"
            )));
        }
        Self::record(
            &mut tx,
            scope,
            "enterprise.sso_session_revoke",
            principal_id,
            &(),
            &serde_json::json!({"principal_id": principal_id, "unlinked": unlink}),
        )
        .await?;
        tx.commit().await
    }

    /// Removes a principal's identities and sessions in the given scopes (deprovisioning).
    pub async fn forget_principal(
        &self,
        cx: &Cx,
        scope_keys: &[String],
        principal_id: &str,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for k in scope_keys {
            tx.delete(&SAML_SESSIONS, k, principal_id).await?;
            tx.delete(&SAML_IDENTITIES, k, principal_id).await?;
        }
        tx.commit().await
    }

    /// Principals whose last assertion in any of `scope_keys` listed `group_id`.
    pub async fn members_of_group(
        tx: &mut Tx,
        scope_keys: &[String],
        group_id: &str,
    ) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for k in scope_keys {
            for (i, _) in scan_all::<SamlIdentity>(tx, &SAML_IDENTITIES, k, None).await? {
                if i.groups.iter().any(|g| g == group_id)
                    && let Some(p) = &i.principal
                {
                    out.insert(p.id.clone());
                }
            }
        }
        Ok(out)
    }

    /// Deletes expired requests, replay-cache entries and sessions. Returns how many.
    pub async fn sweep(&self, cx: &Cx) -> Result<usize> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms() as i64;
        let mut n = 0;
        for t in [&SAML_REQUESTS, &SAML_ASSERTIONS, &SAML_SESSIONS] {
            let rows = tx
                .scan(
                    t,
                    &Scan {
                        n1_max: Some(now),
                        order: crate::store::Order::N1,
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

async fn self_blocked(tx: &mut Tx, principal_id: &str) -> Result<Option<ForgeError>> {
    Ok(tx
        .get_msg::<accounts::PrincipalState>(&accounts::PRINCIPAL_STATES, "", principal_id)
        .await?
        .map(|(s, _)| {
            ForgeError::permission_denied(format!("{principal_id} is {}: {}", s.state, s.reason))
                .with_subcode("principal_blocked")
        }))
}
