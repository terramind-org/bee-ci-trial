//! SCIM 2.0 provisioning (RFC 7643/7644): Users and Groups for an org or an enterprise.
//!
//! [`Scim::handle`] is the protocol layer; the HTTP server routes
//! `/scim/v2/organizations/<org>/...` and `/scim/v2/enterprises/<slug>/...` to it with the
//! method, path, query, body and `Authorization` header, and writes back the status and JSON
//! it returns (`Content-Type: application/scim+json`). Supported: `ServiceProviderConfig`,
//! `ResourceTypes`, `Schemas`; `Users` and `Groups` with `GET` (by id, and lists with
//! `filter`, `startIndex`, `count`), `POST`, `PUT`, `PATCH` ([`super::scim_filter`]) and
//! `DELETE`. Authentication is a bearer token minted by [`Scim::create_token`] (stored as a
//! hash, scoped to one org or enterprise, optionally expiring, revocable).
//!
//! # Mapping
//!
//! - A SCIM user is a [`bee_proto::v1::ScimIdentity`] bound to a principal created on first
//!   provisioning. At org scope the principal also becomes an org member. `userName` is unique
//!   per scope (409 `uniqueness`).
//! - A SCIM group is a named set of users. At org scope, with `provisions_teams` on, creating a
//!   group creates a team linked to it; at enterprise scope groups are linked to teams
//!   explicitly. Membership flows to linked teams ([`super::sync`]).
//! - **Deprovisioning** (`active: false`, or `DELETE`) removes access everywhere in the scope
//!   ([`Scim::deprovision`]): the principal is blocked at admission at once (so every token it
//!   or its agents hold stops working), its SSO sessions and identities go, and it is removed
//!   from every team, every org of the scope, and its direct repo grants there. Setting
//!   `active: true` again lifts the block, re-adds org membership (org scope) and re-applies
//!   group membership.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bee_proto::v1::{
    EnterpriseScope, PrincipalKind, Role, ScimConfig, ScimIdentity, SubjectKind, enterprise_scope,
};
use prost::Message;
use serde_json::{Value, json};

use super::SURFACE;
use super::accounts::{Accounts, kind, put_setting, setting};
use super::common::{
    self, ResolvedScope, ScopeKind, ScopeResolver, audit_enterprise, principal_ref, scan_all,
    scan_page, sha256_hex,
};
use super::scim_filter::{ScimError, apply_patch, get_ci, parse_filter, parse_patch};
use super::sso::Sso;
use super::sync;
use super::teams::{NewTeam, Teams};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::{ts, ts_ms};
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
    SCIM_USERS,
    "forge_scim_users",
    true,
    [K2],
    "SCIM users, scoped by scope key; id the SCIM id, `k1` lower-case userName (unique), `k2` principal id."
);
table!(
    SCIM_USER_EXTRA,
    "forge_scim_user_extra",
    false,
    [],
    "Other SCIM user attributes as JSON, scoped by scope key, id the SCIM id."
);
table!(
    SCIM_GROUPS,
    "forge_scim_groups",
    true,
    [],
    "SCIM groups, scoped by scope key; id the SCIM id, `k1` lower-case displayName (unique)."
);
table!(
    SCIM_TOKENS,
    "forge_scim_tokens",
    false,
    [K1],
    "SCIM bearer tokens, scoped by scope key; `k1` SHA-256 of the token, `n1` expiry (ms)."
);

/// Migration creating the SCIM tables.
pub static M_SCIM: Migration = Migration {
    module: "enterprise",
    version: 6,
    name: "create_scim",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&SCIM_USERS),
        MigrationOp::CreateTable(&SCIM_USER_EXTRA),
        MigrationOp::CreateTable(&SCIM_GROUPS),
        MigrationOp::CreateTable(&SCIM_TOKENS),
    ],
};

const SCHEMA_USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const SCHEMA_GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const SCHEMA_LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const SCHEMA_PATCH: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
/// Most resources returned by one list request.
pub const MAX_COUNT: usize = 200;
/// Default list page size.
pub const DEFAULT_COUNT: usize = 100;

/// A SCIM group record.
#[derive(Clone, PartialEq, Message)]
pub struct GroupRec {
    /// SCIM id.
    #[prost(string, tag = "1")]
    pub id: String,
    /// Display name.
    #[prost(string, tag = "2")]
    pub display_name: String,
    /// The IdP's id for the group.
    #[prost(string, tag = "3")]
    pub external_id: String,
    /// SCIM ids of member users.
    #[prost(string, repeated, tag = "4")]
    pub members: Vec<String>,
    /// Creation time (ms).
    #[prost(uint64, tag = "5")]
    pub created_ms: u64,
    /// Last change (ms).
    #[prost(uint64, tag = "6")]
    pub updated_ms: u64,
}

#[derive(Clone, PartialEq, Message)]
struct TokenRec {
    #[prost(string, tag = "1")]
    description: String,
    #[prost(uint64, tag = "2")]
    created_ms: u64,
    #[prost(bool, tag = "3")]
    revoked: bool,
}

#[derive(Clone, PartialEq, Message)]
struct Extra {
    #[prost(string, tag = "1")]
    json: String,
}

/// An HTTP request to the SCIM endpoint, as the server hands it over.
#[derive(Clone, Debug, Default)]
pub struct ScimRequest {
    /// HTTP method.
    pub method: String,
    /// Full path, `/scim/v2/organizations/<org>/Users/<id>`.
    pub path: String,
    /// Decoded query parameters.
    pub query: BTreeMap<String, String>,
    /// Request body.
    pub body: Vec<u8>,
    /// The `Authorization` header value.
    pub authorization: String,
    /// Source address.
    pub ip: String,
    /// Request id for log correlation.
    pub request_id: String,
}

/// The answer to hand back to the HTTP layer.
#[derive(Clone, Debug, PartialEq)]
pub struct ScimResponse {
    /// HTTP status.
    pub status: u16,
    /// JSON body (`Null` for 204).
    pub body: Value,
    /// `Location` header for created resources.
    pub location: Option<String>,
}

impl ScimResponse {
    fn ok(body: Value) -> ScimResponse {
        ScimResponse {
            status: 200,
            body,
            location: None,
        }
    }

    fn err(e: ScimError) -> ScimResponse {
        ScimResponse {
            status: e.status,
            body: e.to_json(),
            location: None,
        }
    }
}

fn forge_err(e: ForgeError) -> ScimError {
    use bee_proto::v1::ErrorReason as R;
    let (status, ty) = match e.reason {
        R::NotFound => (404, ""),
        R::AlreadyExists => (409, "uniqueness"),
        R::InvalidArgument => (400, "invalidValue"),
        R::PermissionDenied | R::PolicyDenied => (403, ""),
        R::Unauthenticated | R::TokenExpired | R::TokenRevoked => (401, ""),
        R::InvalidState => (409, ""),
        R::Unavailable | R::DeadlineExceeded => (503, ""),
        _ => (500, ""),
    };
    ScimError::new(status, ty, e.message)
}

/// Whether the principal is an active SCIM identity of any of the scopes (`org:<id>` /
/// `enterprise:<id>` keys).
pub async fn principal_provisioned(
    tx: &mut Tx,
    keys: &[String],
    principal_id: &str,
) -> Result<bool> {
    let rows = tx
        .scan_msgs::<ScimIdentity>(
            &SCIM_USERS,
            &Scan {
                key: Some(Key::K2(principal_id.to_string())),
                limit: 100,
                ..Scan::default()
            },
        )
        .await?;
    Ok(rows
        .iter()
        .any(|(u, r)| u.active && keys.contains(&r.scope)))
}

/// What a deprovisioning removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeprovisionReport {
    /// Teams the principal left.
    pub teams_left: Vec<String>,
    /// Orgs the principal was removed from.
    pub orgs_left: Vec<String>,
    /// Orgs where the principal is the last owner and so stays a member (blocked at admission).
    pub retained_owner_of: Vec<String>,
    /// Repos whose direct grants were removed.
    pub grants_removed: Vec<String>,
}

#[derive(Debug)]
struct UserFields {
    user_name: String,
    external_id: String,
    display_name: String,
    emails: Vec<String>,
    active: bool,
    extra: Value,
}

/// SCIM provisioning over the enterprise surface.
pub struct Scim {
    db: Db,
    forge: Arc<Forge>,
    resolver: Arc<dyn ScopeResolver>,
    accounts: Arc<Accounts>,
    sso: Arc<Sso>,
    teams: Arc<Teams>,
    base_url: String,
}

impl Scim {
    /// SCIM over the forge's store; `base_url` is this deployment's public URL.
    pub fn new(
        forge: Arc<Forge>,
        resolver: Arc<dyn ScopeResolver>,
        accounts: Arc<Accounts>,
        sso: Arc<Sso>,
        teams: Arc<Teams>,
        base_url: &str,
    ) -> Scim {
        Scim {
            db: forge.db.clone(),
            forge,
            resolver,
            accounts,
            sso,
            teams,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// The SCIM base URL of a scope.
    pub fn endpoint_url(&self, scope: &ResolvedScope) -> String {
        let seg = match scope.kind {
            ScopeKind::Org => "organizations",
            ScopeKind::Enterprise => "enterprises",
        };
        format!("{}/scim/v2/{seg}/{}", self.base_url, scope.name)
    }

    /// Scope keys whose SCIM data belongs to the scope: its own and its orgs'.
    fn keys(scope: &ResolvedScope) -> Vec<String> {
        let mut k = vec![scope.key()];
        if scope.kind == ScopeKind::Enterprise {
            k.extend(scope.org_ids.iter().map(|o| format!("org:{o}")));
        }
        k
    }

    // ------------------------------------------------------------------ configuration

    /// The SCIM configuration of a scope.
    pub async fn config(&self, cx: &Cx, scope: &ResolvedScope) -> Result<ScimConfig> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut c = setting::<ScimConfig>(&mut tx, &key, kind::SCIM)
            .await?
            .map(|(c, _)| c)
            .unwrap_or_default();
        let now = tx.now_ms() as i64;
        c.scope = Some(scope.to_proto());
        c.endpoint_url = self.endpoint_url(scope);
        c.active_token_count = scan_all::<TokenRec>(&mut tx, &SCIM_TOKENS, &key, None)
            .await?
            .into_iter()
            .filter(|(t, r)| !t.revoked && r.n1.unwrap_or(i64::MAX) > now)
            .count() as u32;
        c.provisioned_count = tx.count(&SCIM_USERS, &key, None).await?;
        Ok(c)
    }

    /// Enables or disables SCIM, or team provisioning from groups.
    pub async fn update_config(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        enabled: Option<bool>,
        provisions_teams: Option<bool>,
    ) -> Result<ScimConfig> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let before = setting::<ScimConfig>(&mut tx, &key, kind::SCIM)
            .await?
            .map(|(c, _)| c)
            .unwrap_or_default();
        let mut c = before.clone();
        if let Some(v) = enabled {
            c.enabled = v;
        }
        if let Some(v) = provisions_teams {
            c.provisions_teams = v;
        }
        put_setting(&mut tx, &key, kind::SCIM, &c).await?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_config_update",
            &key,
            &before,
            &c,
        )
        .await?;
        tx.commit().await?;
        self.config(cx, scope).await
    }

    async fn audit<B: serde::Serialize, A: serde::Serialize>(
        &self,
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

    /// Mints a bearer token for the IdP (shown once). `ttl_seconds` 0 means no expiry.
    pub async fn create_token(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        description: &str,
        ttl_seconds: u32,
    ) -> Result<(String, String, Option<u64>)> {
        let secret = self.forge.db.ids().random_hex(32)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = tx.new_id()?;
        let token = format!("scim_{id}_{secret}");
        let now = tx.now_ms();
        let expires = (ttl_seconds > 0).then(|| now + u64::from(ttl_seconds) * 1000);
        let rec = TokenRec {
            description: description.into(),
            created_ms: now,
            revoked: false,
        };
        tx.insert(
            &SCIM_TOKENS,
            &Row::new(scope.key(), id.clone())
                .k1(sha256_hex(&token))
                .n1(expires.map(|e| e as i64).unwrap_or(i64::MAX))
                .msg(&rec),
        )
        .await?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_token_create",
            &id,
            &(),
            &json!({"token_id": id, "description": description, "expires_ms": expires}),
        )
        .await?;
        tx.commit().await?;
        Ok((token, id, expires))
    }

    /// Revokes a token.
    pub async fn revoke_token(&self, cx: &Cx, scope: &ResolvedScope, token_id: &str) -> Result<()> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) =
            common::get_or_404::<TokenRec>(&mut tx, &SCIM_TOKENS, &key, token_id, "SCIM token")
                .await?;
        rec.revoked = true;
        tx.update(&SCIM_TOKENS, &row.msg(&rec)).await?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_token_revoke",
            token_id,
            &(),
            &json!({"token_id": token_id}),
        )
        .await?;
        tx.commit().await
    }

    /// Lists SCIM identities of a scope.
    pub async fn list_identities(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        query: &str,
        include_inactive: bool,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ScimIdentity>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) =
            scan_page::<ScimIdentity>(&mut tx, &SCIM_USERS, &scope.key(), None, after, limit)
                .await?;
        let q = query.to_lowercase();
        Ok((
            rows.into_iter()
                .map(|(u, _)| u)
                .filter(|u| include_inactive || u.active)
                .filter(|u| {
                    q.is_empty()
                        || u.user_name.to_lowercase().contains(&q)
                        || u.display_name.to_lowercase().contains(&q)
                        || u.emails.iter().any(|e| e.to_lowercase().contains(&q))
                })
                .collect(),
            next,
        ))
    }

    // ------------------------------------------------------------------ HTTP protocol

    /// Serves one SCIM HTTP request.
    pub async fn handle(&self, req: ScimRequest) -> ScimResponse {
        match self.dispatch(&req).await {
            Ok(r) => r,
            Err(e) => ScimResponse::err(e),
        }
    }

    async fn dispatch(&self, req: &ScimRequest) -> std::result::Result<ScimResponse, ScimError> {
        let rest = req
            .path
            .split('?')
            .next()
            .unwrap_or("")
            .strip_prefix("/scim/v2/")
            .ok_or_else(|| ScimError::new(404, "", "not a SCIM path"))?;
        let seg: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        let (kind_seg, name, resource, id) = match seg.as_slice() {
            [k, n, r] => (*k, *n, *r, None),
            [k, n, r, i] => (*k, *n, *r, Some(*i)),
            _ => return Err(ScimError::new(404, "", "unknown SCIM resource")),
        };
        let sys = Cx::new("scim", PrincipalKind::Service);
        let scope = match kind_seg {
            "organizations" => self.resolver.resolve_org(&sys, name).await,
            "enterprises" => {
                self.resolver
                    .resolve(
                        &sys,
                        &Some(EnterpriseScope {
                            scope: Some(enterprise_scope::Scope::Enterprise(name.to_string())),
                        }),
                    )
                    .await
            }
            _ => return Err(ScimError::new(404, "", "unknown SCIM scope")),
        }
        .map_err(|_| ScimError::new(404, "", "unknown SCIM scope"))?;
        let token_id = self.authenticate(&sys, &scope, &req.authorization).await?;
        let mut cx = Cx::new(format!("scim:{token_id}"), PrincipalKind::Service);
        cx.decision = format!("scim token {token_id}");
        cx.ip = req.ip.clone();
        cx.request_id = req.request_id.clone();
        cx.user_agent = "scim".into();
        {
            let mut tx = self.db.begin(SURFACE, &cx).await.map_err(forge_err)?;
            let key = scope.key();
            let mut c = setting::<ScimConfig>(&mut tx, &key, kind::SCIM)
                .await
                .map_err(forge_err)?
                .map(|(c, _)| c)
                .unwrap_or_default();
            c.last_request_at = Some(ts(tx.now_ms()));
            put_setting(&mut tx, &key, kind::SCIM, &c)
                .await
                .map_err(forge_err)?;
            tx.commit().await.map_err(forge_err)?;
        }
        let method = req.method.to_ascii_uppercase();
        let body = || -> std::result::Result<Value, ScimError> {
            serde_json::from_slice(&req.body)
                .map_err(|e| ScimError::new(400, "invalidSyntax", format!("body is not JSON: {e}")))
        };
        match (resource, id, method.as_str()) {
            ("ServiceProviderConfig", None, "GET") => {
                Ok(ScimResponse::ok(service_provider_config()))
            }
            ("ResourceTypes", None, "GET") => Ok(ScimResponse::ok(resource_types())),
            ("Schemas", None, "GET") => Ok(ScimResponse::ok(schemas())),
            ("Users", None, "GET") => self.list(&cx, &scope, req, true).await,
            ("Users", Some(i), "GET") => self.get_user(&cx, &scope, i).await.map(ScimResponse::ok),
            ("Users", None, "POST") => self.post_user(&cx, &scope, &body()?).await,
            ("Users", Some(i), "PUT") => self
                .put_user(&cx, &scope, i, &body()?)
                .await
                .map(ScimResponse::ok),
            ("Users", Some(i), "PATCH") => self
                .patch_user(&cx, &scope, i, &body()?)
                .await
                .map(ScimResponse::ok),
            ("Users", Some(i), "DELETE") => self.delete_user(&cx, &scope, i).await,
            ("Groups", None, "GET") => self.list(&cx, &scope, req, false).await,
            ("Groups", Some(i), "GET") => {
                self.get_group(&cx, &scope, i).await.map(ScimResponse::ok)
            }
            ("Groups", None, "POST") => self.post_group(&cx, &scope, &body()?).await,
            ("Groups", Some(i), "PUT") => self
                .put_group(&cx, &scope, i, &body()?)
                .await
                .map(ScimResponse::ok),
            ("Groups", Some(i), "PATCH") => self
                .patch_group(&cx, &scope, i, &body()?)
                .await
                .map(ScimResponse::ok),
            ("Groups", Some(i), "DELETE") => self.delete_group(&cx, &scope, i).await,
            _ => Err(ScimError::new(
                if matches!(
                    resource,
                    "Users" | "Groups" | "ServiceProviderConfig" | "ResourceTypes" | "Schemas"
                ) {
                    405
                } else {
                    404
                },
                "",
                "unsupported method or resource",
            )),
        }
    }

    async fn authenticate(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        header: &str,
    ) -> std::result::Result<String, ScimError> {
        let unauth = |m: &str| ScimError::new(401, "", m.to_string());
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .ok_or_else(|| unauth("a bearer token is required"))?
            .trim();
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let rows = tx
            .scan_msgs::<TokenRec>(
                &SCIM_TOKENS,
                &Scan {
                    key: Some(Key::K1(sha256_hex(token))),
                    limit: 2,
                    ..Scan::default()
                },
            )
            .await
            .map_err(forge_err)?;
        let now = tx.now_ms() as i64;
        let Some((rec, row)) = rows.into_iter().next() else {
            return Err(unauth("unknown token"));
        };
        if row.scope != scope.key() || rec.revoked || row.n1.unwrap_or(i64::MAX) <= now {
            return Err(unauth("the token is revoked, expired or for another scope"));
        }
        let enabled = setting::<ScimConfig>(&mut tx, &scope.key(), kind::SCIM)
            .await
            .map_err(forge_err)?
            .is_some_and(|(c, _)| c.enabled);
        if !enabled {
            return Err(ScimError::new(403, "", "SCIM provisioning is disabled"));
        }
        Ok(row.id)
    }

    // ------------------------------------------------------------------ rendering

    fn location(&self, scope: &ResolvedScope, res: &str, id: &str) -> String {
        format!("{}/{res}/{id}", self.endpoint_url(scope))
    }

    async fn user_json(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
        u: &ScimIdentity,
    ) -> std::result::Result<Value, ScimError> {
        let key = scope.key();
        let extra: Value = tx
            .get_msg::<Extra>(&SCIM_USER_EXTRA, &key, &u.scim_id)
            .await
            .map_err(forge_err)?
            .and_then(|(e, _)| serde_json::from_str(&e.json).ok())
            .unwrap_or_else(|| json!({}));
        let mut o = match extra {
            Value::Object(m) => m,
            _ => Default::default(),
        };
        o.insert("schemas".into(), json!([SCHEMA_USER]));
        o.insert("id".into(), json!(u.scim_id));
        if !u.external_id.is_empty() {
            o.insert("externalId".into(), json!(u.external_id));
        }
        o.insert("userName".into(), json!(u.user_name));
        if !u.display_name.is_empty() {
            o.insert("displayName".into(), json!(u.display_name));
        }
        if !o.contains_key("emails") && !u.emails.is_empty() {
            let emails: Vec<Value> = u
                .emails
                .iter()
                .enumerate()
                .map(|(i, e)| json!({"value": e, "primary": i == 0}))
                .collect();
            o.insert("emails".into(), json!(emails));
        }
        o.insert("active".into(), json!(u.active));
        let mut groups = Vec::new();
        for (g, _) in scan_all::<GroupRec>(tx, &SCIM_GROUPS, &key, None)
            .await
            .map_err(forge_err)?
        {
            if g.members.contains(&u.scim_id) {
                groups.push(json!({
                    "value": g.id, "display": g.display_name,
                    "$ref": self.location(scope, "Groups", &g.id), "type": "direct"
                }));
            }
        }
        o.insert("groups".into(), json!(groups));
        o.insert(
            "meta".into(),
            json!({
                "resourceType": "User",
                "created": crate::enterprise::saml::format_instant(u.created_at.as_ref().map(ts_ms).unwrap_or(0) / 1000),
                "lastModified": crate::enterprise::saml::format_instant(u.updated_at.as_ref().map(ts_ms).unwrap_or(0) / 1000),
                "location": self.location(scope, "Users", &u.scim_id),
            }),
        );
        Ok(Value::Object(o))
    }

    async fn group_json(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
        g: &GroupRec,
    ) -> std::result::Result<Value, ScimError> {
        let key = scope.key();
        let mut members = Vec::new();
        for uid in &g.members {
            if let Some((u, _)) = tx
                .get_msg::<ScimIdentity>(&SCIM_USERS, &key, uid)
                .await
                .map_err(forge_err)?
            {
                members.push(json!({
                    "value": uid, "display": u.user_name,
                    "$ref": self.location(scope, "Users", uid), "type": "User"
                }));
            }
        }
        let mut o = json!({
            "schemas": [SCHEMA_GROUP],
            "id": g.id,
            "displayName": g.display_name,
            "members": members,
            "meta": {
                "resourceType": "Group",
                "created": crate::enterprise::saml::format_instant(g.created_ms / 1000),
                "lastModified": crate::enterprise::saml::format_instant(g.updated_ms / 1000),
                "location": self.location(scope, "Groups", &g.id),
            }
        });
        if !g.external_id.is_empty() {
            o["externalId"] = json!(g.external_id);
        }
        Ok(o)
    }

    // ------------------------------------------------------------------ lists

    async fn list(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        req: &ScimRequest,
        users: bool,
    ) -> std::result::Result<ScimResponse, ScimError> {
        let filter = req
            .query
            .get("filter")
            .filter(|f| !f.is_empty())
            .map(|f| parse_filter(f))
            .transpose()?;
        let start = req
            .query
            .get("startIndex")
            .map(|s| {
                s.parse::<i64>()
                    .map_err(|_| ScimError::new(400, "invalidValue", "bad startIndex"))
            })
            .transpose()?
            .unwrap_or(1)
            .max(1) as usize;
        let count = req
            .query
            .get("count")
            .map(|s| {
                s.parse::<i64>()
                    .map_err(|_| ScimError::new(400, "invalidValue", "bad count"))
            })
            .transpose()?
            .unwrap_or(DEFAULT_COUNT as i64)
            .clamp(0, MAX_COUNT as i64) as usize;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let mut all: Vec<Value> = Vec::new();
        if users {
            for (u, _) in scan_all::<ScimIdentity>(&mut tx, &SCIM_USERS, &key, None)
                .await
                .map_err(forge_err)?
            {
                let j = self.user_json(&mut tx, scope, &u).await?;
                if filter.as_ref().is_none_or(|f| f.matches(&j)) {
                    all.push(j);
                }
            }
        } else {
            for (g, _) in scan_all::<GroupRec>(&mut tx, &SCIM_GROUPS, &key, None)
                .await
                .map_err(forge_err)?
            {
                let j = self.group_json(&mut tx, scope, &g).await?;
                if filter.as_ref().is_none_or(|f| f.matches(&j)) {
                    all.push(j);
                }
            }
        }
        let total = all.len();
        let page: Vec<Value> = all.into_iter().skip(start - 1).take(count).collect();
        Ok(ScimResponse::ok(json!({
            "schemas": [SCHEMA_LIST],
            "totalResults": total,
            "startIndex": start,
            "itemsPerPage": page.len(),
            "Resources": page,
        })))
    }

    // ------------------------------------------------------------------ users

    async fn load_user(
        tx: &mut Tx,
        key: &str,
        id: &str,
    ) -> std::result::Result<(ScimIdentity, Row), ScimError> {
        tx.get_msg::<ScimIdentity>(&SCIM_USERS, key, id)
            .await
            .map_err(forge_err)?
            .ok_or_else(|| ScimError::new(404, "", format!("user {id} not found")))
    }

    async fn get_user(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
    ) -> std::result::Result<Value, ScimError> {
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let (u, _) = Self::load_user(&mut tx, &scope.key(), id).await?;
        self.user_json(&mut tx, scope, &u).await
    }

    fn ingest_user(v: &Value) -> std::result::Result<UserFields, ScimError> {
        let o = v
            .as_object()
            .ok_or_else(|| ScimError::new(400, "invalidSyntax", "a user is a JSON object"))?;
        let user_name = get_ci(o, "userName")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ScimError::new(400, "invalidValue", "userName is required"))?
            .trim()
            .to_string();
        if user_name.len() > 256 {
            return Err(ScimError::new(400, "invalidValue", "userName is too long"));
        }
        let active = match get_ci(o, "active") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => true,
            Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => false,
            Some(_) => {
                return Err(ScimError::new(
                    400,
                    "invalidValue",
                    "active must be a boolean",
                ));
            }
        };
        let emails: Vec<String> = match get_ci(o, "emails") {
            Some(Value::Array(a)) => {
                let mut primary: Vec<String> = Vec::new();
                let mut rest: Vec<String> = Vec::new();
                for e in a {
                    let Some(val) = e.get("value").and_then(Value::as_str) else {
                        return Err(ScimError::new(
                            400,
                            "invalidValue",
                            "an email needs a value",
                        ));
                    };
                    if e.get("primary").and_then(Value::as_bool).unwrap_or(false) {
                        primary.push(val.to_string());
                    } else {
                        rest.push(val.to_string());
                    }
                }
                primary.extend(rest);
                primary
            }
            None | Some(Value::Null) => Vec::new(),
            Some(_) => return Err(ScimError::new(400, "invalidValue", "emails must be a list")),
        };
        let display_name = get_ci(o, "displayName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                get_ci(o, "name")
                    .and_then(|n| n.get("formatted"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let mut extra = o.clone();
        for k in [
            "schemas",
            "id",
            "meta",
            "groups",
            "userName",
            "externalId",
            "displayName",
            "active",
        ] {
            if let Some(key) = extra.keys().find(|x| x.eq_ignore_ascii_case(k)).cloned() {
                extra.remove(&key);
            }
        }
        Ok(UserFields {
            user_name,
            external_id: get_ci(o, "externalId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            display_name,
            emails,
            active,
            extra: Value::Object(extra),
        })
    }

    async fn post_user(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        body: &Value,
    ) -> std::result::Result<ScimResponse, ScimError> {
        let f = Self::ingest_user(body)?;
        let key = scope.key();
        let shortcode = if scope.kind == ScopeKind::Enterprise {
            self.accounts
                .get(cx, &scope.id)
                .await
                .map_err(forge_err)?
                .managed_users_shortcode
        } else {
            String::new()
        };
        let principal_id = self.forge.db.ids().ulid().map_err(forge_err)?;
        let display = if f.display_name.is_empty() {
            f.user_name.clone()
        } else {
            f.display_name.clone()
        };
        let mut principal = principal_ref(&principal_id, PrincipalKind::Human);
        principal.display = format!("{}{shortcode}", f.user_name);
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        if !tx
            .scan(
                &SCIM_USERS,
                &Scan::scope(key.clone(), 1).key(Key::K1(f.user_name.to_lowercase())),
            )
            .await
            .map_err(forge_err)?
            .is_empty()
        {
            return Err(ScimError::new(
                409,
                "uniqueness",
                format!("userName {} is taken", f.user_name),
            ));
        }
        let now = tx.now_ms();
        let u = ScimIdentity {
            scim_id: tx.new_id().map_err(forge_err)?,
            external_id: f.external_id.clone(),
            principal: Some(principal.clone()),
            user_name: f.user_name.clone(),
            display_name: display,
            emails: f.emails.clone(),
            active: true,
            groups: Vec::new(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        tx.insert(
            &SCIM_USERS,
            &Row::new(key.clone(), u.scim_id.clone())
                .k1(f.user_name.to_lowercase())
                .k2(principal_id.clone())
                .msg(&u),
        )
        .await
        .map_err(forge_err)?;
        tx.insert(
            &SCIM_USER_EXTRA,
            &Row::new(key.clone(), u.scim_id.clone()).msg(&Extra {
                json: f.extra.to_string(),
            }),
        )
        .await
        .map_err(forge_err)?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_user_create",
            &u.scim_id,
            &(),
            &json!({
                "scim_id": u.scim_id, "principal_id": principal_id, "user_name": u.user_name
            }),
        )
        .await
        .map_err(forge_err)?;
        tx.commit().await.map_err(forge_err)?;
        self.forge.hooks.directory.observe(&principal);
        if scope.kind == ScopeKind::Org {
            self.forge
                .orgs
                .set_member(cx, &scope.id, principal.clone(), Role::Member)
                .await
                .map_err(forge_err)?;
            let _ = self
                .teams
                .resolve_pending(
                    cx,
                    &self
                        .forge
                        .orgs
                        .org_any(cx, &scope.id)
                        .await
                        .map_err(forge_err)?,
                    &principal_id,
                    &f.emails,
                )
                .await;
        }
        if !f.active {
            self.deprovision(cx, scope, &u).await.map_err(forge_err)?;
            let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
            let (mut cur, row) = Self::load_user(&mut tx, &key, &u.scim_id).await?;
            cur.active = false;
            tx.update(&SCIM_USERS, &row.msg(&cur))
                .await
                .map_err(forge_err)?;
            tx.commit().await.map_err(forge_err)?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let (cur, _) = Self::load_user(&mut tx, &key, &u.scim_id).await?;
        let body = self.user_json(&mut tx, scope, &cur).await?;
        Ok(ScimResponse {
            status: 201,
            body,
            location: Some(self.location(scope, "Users", &u.scim_id)),
        })
    }

    /// Applies new field values to a stored user, handling activation changes.
    async fn save_user(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        f: UserFields,
    ) -> std::result::Result<Value, ScimError> {
        let key = scope.key();
        let (before, after) = {
            let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
            let (mut u, row) = Self::load_user(&mut tx, &key, id).await?;
            let before = u.clone();
            let lower = f.user_name.to_lowercase();
            if lower != before.user_name.to_lowercase()
                && !tx
                    .scan(
                        &SCIM_USERS,
                        &Scan::scope(key.clone(), 1).key(Key::K1(lower.clone())),
                    )
                    .await
                    .map_err(forge_err)?
                    .is_empty()
            {
                return Err(ScimError::new(
                    409,
                    "uniqueness",
                    format!("userName {} is taken", f.user_name),
                ));
            }
            u.user_name = f.user_name.clone();
            u.external_id = f.external_id.clone();
            u.display_name = if f.display_name.is_empty() {
                f.user_name.clone()
            } else {
                f.display_name.clone()
            };
            u.emails = f.emails.clone();
            u.active = f.active;
            u.updated_at = Some(ts(tx.now_ms()));
            let k2 = row.k2.clone().unwrap_or_default();
            tx.update(
                &SCIM_USERS,
                &Row {
                    k1: Some(lower),
                    k2: Some(k2),
                    ..row
                }
                .msg(&u),
            )
            .await
            .map_err(forge_err)?;
            tx.put(
                &SCIM_USER_EXTRA,
                &Row::new(key.clone(), id.to_string()).msg(&Extra {
                    json: f.extra.to_string(),
                }),
            )
            .await
            .map_err(forge_err)?;
            self.audit(
                &mut tx,
                scope,
                "enterprise.scim_user_update",
                id,
                &json!({
                    "user_name": before.user_name, "active": before.active
                }),
                &json!({"user_name": u.user_name, "active": u.active}),
            )
            .await
            .map_err(forge_err)?;
            tx.commit().await.map_err(forge_err)?;
            (before, u)
        };
        if before.active && !after.active {
            self.deprovision(cx, scope, &after)
                .await
                .map_err(forge_err)?;
        } else if !before.active && after.active {
            self.reactivate(cx, scope, &after)
                .await
                .map_err(forge_err)?;
        }
        self.get_user(cx, scope, id).await
    }

    async fn put_user(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        body: &Value,
    ) -> std::result::Result<Value, ScimError> {
        let f = Self::ingest_user(body)?;
        self.save_user(cx, scope, id, f).await
    }

    async fn patch_user(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        body: &Value,
    ) -> std::result::Result<Value, ScimError> {
        check_patch_schema(body)?;
        let ops = parse_patch(body)?;
        let mut current = self.get_user(cx, scope, id).await?;
        apply_patch(&mut current, &ops)?;
        let f = Self::ingest_user(&current)?;
        self.save_user(cx, scope, id, f).await
    }

    async fn delete_user(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
    ) -> std::result::Result<ScimResponse, ScimError> {
        let key = scope.key();
        let u = {
            let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
            Self::load_user(&mut tx, &key, id).await?.0
        };
        self.deprovision(cx, scope, &u).await.map_err(forge_err)?;
        let mut groups_touched = BTreeSet::new();
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        for (mut g, row) in scan_all::<GroupRec>(&mut tx, &SCIM_GROUPS, &key, None)
            .await
            .map_err(forge_err)?
        {
            if g.members.contains(&u.scim_id) {
                g.members.retain(|m| *m != u.scim_id);
                tx.update(&SCIM_GROUPS, &row.msg(&g))
                    .await
                    .map_err(forge_err)?;
                groups_touched.insert(g.id.clone());
            }
        }
        tx.delete(&SCIM_USERS, &key, id).await.map_err(forge_err)?;
        tx.delete(&SCIM_USER_EXTRA, &key, id)
            .await
            .map_err(forge_err)?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_user_delete",
            id,
            &json!({"user_name": u.user_name}),
            &(),
        )
        .await
        .map_err(forge_err)?;
        tx.commit().await.map_err(forge_err)?;
        sync::resync_groups(&self.forge, cx, "scim", &groups_touched)
            .await
            .map_err(forge_err)?;
        Ok(ScimResponse {
            status: 204,
            body: Value::Null,
            location: None,
        })
    }

    // ------------------------------------------------------------------ deprovisioning

    /// Removes the principal's access everywhere in the scope (see the module docs). Safe to
    /// repeat.
    pub async fn deprovision(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        u: &ScimIdentity,
    ) -> Result<DeprovisionReport> {
        let Some(p) = &u.principal else {
            return Ok(DeprovisionReport::default());
        };
        let pid = p.id.clone();
        self.accounts
            .set_principal_state(
                cx,
                &pid,
                Some(("deprovisioned", "removed by the identity provider")),
            )
            .await?;
        self.sso
            .forget_principal(cx, &Self::keys(scope), &pid)
            .await?;
        let mut report = DeprovisionReport {
            teams_left: self
                .teams
                .remove_principal_everywhere(cx, &scope.org_ids, &pid)
                .await?,
            ..Default::default()
        };
        for org_id in &scope.org_ids {
            if self.forge.orgs.member(cx, org_id, &pid).await?.is_some() {
                match self
                    .forge
                    .orgs
                    .set_member(cx, org_id, p.clone(), Role::Unspecified)
                    .await
                {
                    Ok(_) => report.orgs_left.push(org_id.clone()),
                    Err(e) if e.reason == bee_proto::v1::ErrorReason::InvalidState => {
                        report.retained_owner_of.push(org_id.clone());
                    }
                    Err(e) => return Err(e),
                }
            }
            let mut after = String::new();
            loop {
                let (repos, next) = self.forge.orgs.list_repos(cx, org_id, &after, 500).await?;
                for repo in repos {
                    match self
                        .forge
                        .orgs
                        .remove_collaborator(cx, &repo.id, &pid)
                        .await
                    {
                        Ok(()) => report.grants_removed.push(repo.id),
                        Err(e) if e.reason == bee_proto::v1::ErrorReason::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
                if next.is_empty() {
                    break;
                }
                after = next;
            }
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_deprovision",
            &pid,
            &(),
            &json!({
                "principal_id": pid, "teams_left": report.teams_left,
                "orgs_left": report.orgs_left, "retained_owner_of": report.retained_owner_of,
                "grants_removed": report.grants_removed,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(report)
    }

    async fn reactivate(&self, cx: &Cx, scope: &ResolvedScope, u: &ScimIdentity) -> Result<()> {
        let Some(p) = &u.principal else { return Ok(()) };
        self.accounts.set_principal_state(cx, &p.id, None).await?;
        if scope.kind == ScopeKind::Org
            && self
                .forge
                .orgs
                .member(cx, &scope.id, &p.id)
                .await?
                .is_none()
        {
            self.forge
                .orgs
                .set_member(cx, &scope.id, p.clone(), Role::Member)
                .await?;
        }
        let groups: BTreeSet<String> = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let mut out = BTreeSet::new();
            for k in Self::keys(scope) {
                for (g, _) in scan_all::<GroupRec>(&mut tx, &SCIM_GROUPS, &k, None).await? {
                    if g.members.contains(&u.scim_id) {
                        out.insert(g.id);
                    }
                }
            }
            out
        };
        sync::resync_groups(&self.forge, cx, "scim", &groups).await?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_reactivate",
            &p.id,
            &(),
            &json!({"principal_id": p.id}),
        )
        .await?;
        tx.commit().await
    }

    // ------------------------------------------------------------------ groups

    async fn load_group(
        tx: &mut Tx,
        key: &str,
        id: &str,
    ) -> std::result::Result<(GroupRec, Row), ScimError> {
        tx.get_msg::<GroupRec>(&SCIM_GROUPS, key, id)
            .await
            .map_err(forge_err)?
            .ok_or_else(|| ScimError::new(404, "", format!("group {id} not found")))
    }

    async fn get_group(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
    ) -> std::result::Result<Value, ScimError> {
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let (g, _) = Self::load_group(&mut tx, &scope.key(), id).await?;
        self.group_json(&mut tx, scope, &g).await
    }

    async fn ingest_group(
        &self,
        tx: &mut Tx,
        key: &str,
        v: &Value,
    ) -> std::result::Result<(String, String, Vec<String>), ScimError> {
        let o = v
            .as_object()
            .ok_or_else(|| ScimError::new(400, "invalidSyntax", "a group is a JSON object"))?;
        let name = get_ci(o, "displayName")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ScimError::new(400, "invalidValue", "displayName is required"))?
            .trim()
            .to_string();
        let mut members = Vec::new();
        if let Some(Value::Array(a)) = get_ci(o, "members") {
            for m in a {
                let id = m
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ScimError::new(400, "invalidValue", "a member needs a value"))?;
                if tx
                    .get(&SCIM_USERS, key, id)
                    .await
                    .map_err(forge_err)?
                    .is_none()
                {
                    return Err(ScimError::new(
                        400,
                        "invalidValue",
                        format!("member {id} is not a user"),
                    ));
                }
                if !members.iter().any(|x| x == id) {
                    members.push(id.to_string());
                }
            }
        }
        let ext = get_ci(o, "externalId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok((name, ext, members))
    }

    async fn post_group(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        body: &Value,
    ) -> std::result::Result<ScimResponse, ScimError> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let (name, ext, members) = self.ingest_group(&mut tx, &key, body).await?;
        if !tx
            .scan(
                &SCIM_GROUPS,
                &Scan::scope(key.clone(), 1).key(Key::K1(name.to_lowercase())),
            )
            .await
            .map_err(forge_err)?
            .is_empty()
        {
            return Err(ScimError::new(
                409,
                "uniqueness",
                format!("group {name} exists"),
            ));
        }
        let now = tx.now_ms();
        let g = GroupRec {
            id: tx.new_id().map_err(forge_err)?,
            display_name: name.clone(),
            external_id: ext,
            members,
            created_ms: now,
            updated_ms: now,
        };
        tx.insert(
            &SCIM_GROUPS,
            &Row::new(key.clone(), g.id.clone())
                .k1(name.to_lowercase())
                .msg(&g),
        )
        .await
        .map_err(forge_err)?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_group_create",
            &g.id,
            &(),
            &json!({
                "id": g.id, "display_name": g.display_name, "members": g.members.len()
            }),
        )
        .await
        .map_err(forge_err)?;
        tx.commit().await.map_err(forge_err)?;
        let provisions = {
            let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
            setting::<ScimConfig>(&mut tx, &key, kind::SCIM)
                .await
                .map_err(forge_err)?
                .is_some_and(|(c, _)| c.provisions_teams)
        };
        if provisions && scope.kind == ScopeKind::Org {
            self.provision_team(cx, scope, &g)
                .await
                .map_err(forge_err)?;
        }
        sync::resync_groups(&self.forge, cx, "scim", &BTreeSet::from([g.id.clone()]))
            .await
            .map_err(forge_err)?;
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let body = self.group_json(&mut tx, scope, &g).await?;
        Ok(ScimResponse {
            status: 201,
            body,
            location: Some(self.location(scope, "Groups", &g.id)),
        })
    }

    async fn provision_team(&self, cx: &Cx, scope: &ResolvedScope, g: &GroupRec) -> Result<()> {
        let org = self.forge.orgs.org_any(cx, &scope.id).await?;
        let base = {
            let s = common::slugify(&g.display_name);
            if s.is_empty() { "group".to_string() } else { s }
        };
        for n in 0..50 {
            let slug = if n == 0 {
                base.clone()
            } else {
                format!("{base}-{}", n + 1)
            };
            match self
                .teams
                .create(
                    cx,
                    &org,
                    NewTeam {
                        slug: slug.clone(),
                        name: g.display_name.clone(),
                        description: "Provisioned from an IdP group".into(),
                        no_maintainers: true,
                        ..NewTeam::default()
                    },
                )
                .await
            {
                Ok(_) => {
                    self.teams
                        .link_group(cx, &org, &slug, "scim", &g.id, &g.display_name)
                        .await?;
                    return Ok(());
                }
                Err(e) if e.reason == bee_proto::v1::ErrorReason::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(ForgeError::already_exists(format!(
            "a team for group {}",
            g.display_name
        )))
    }

    async fn save_group(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        v: &Value,
    ) -> std::result::Result<Value, ScimError> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let (mut g, row) = Self::load_group(&mut tx, &key, id).await?;
        let (name, ext, members) = self.ingest_group(&mut tx, &key, v).await?;
        let lower = name.to_lowercase();
        if lower != g.display_name.to_lowercase()
            && !tx
                .scan(
                    &SCIM_GROUPS,
                    &Scan::scope(key.clone(), 1).key(Key::K1(lower.clone())),
                )
                .await
                .map_err(forge_err)?
                .is_empty()
        {
            return Err(ScimError::new(
                409,
                "uniqueness",
                format!("group {name} exists"),
            ));
        }
        let before = json!({"display_name": g.display_name, "members": g.members.len()});
        g.display_name = name;
        g.external_id = ext;
        g.members = members;
        g.updated_ms = tx.now_ms();
        tx.update(
            &SCIM_GROUPS,
            &Row {
                k1: Some(lower),
                ..row
            }
            .msg(&g),
        )
        .await
        .map_err(forge_err)?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_group_update",
            id,
            &before,
            &json!({
                "display_name": g.display_name, "members": g.members.len()
            }),
        )
        .await
        .map_err(forge_err)?;
        tx.commit().await.map_err(forge_err)?;
        sync::resync_groups(&self.forge, cx, "scim", &BTreeSet::from([id.to_string()]))
            .await
            .map_err(forge_err)?;
        self.get_group(cx, scope, id).await
    }

    async fn put_group(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        body: &Value,
    ) -> std::result::Result<Value, ScimError> {
        self.save_group(cx, scope, id, body).await
    }

    async fn patch_group(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        body: &Value,
    ) -> std::result::Result<Value, ScimError> {
        check_patch_schema(body)?;
        let ops = parse_patch(body)?;
        let mut current = self.get_group(cx, scope, id).await?;
        apply_patch(&mut current, &ops)?;
        self.save_group(cx, scope, id, &current).await
    }

    async fn delete_group(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
    ) -> std::result::Result<ScimResponse, ScimError> {
        let key = scope.key();
        let g = {
            let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
            Self::load_group(&mut tx, &key, id).await?.0
        };
        // Empty the group first so linked teams drop its members, then unlink and delete.
        let linked = self
            .teams
            .teams_linked_to(cx, "scim", id)
            .await
            .map_err(forge_err)?;
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        let (mut emptied, row) = Self::load_group(&mut tx, &key, id).await?;
        emptied.members.clear();
        tx.update(&SCIM_GROUPS, &row.msg(&emptied))
            .await
            .map_err(forge_err)?;
        tx.commit().await.map_err(forge_err)?;
        sync::resync_groups(&self.forge, cx, "scim", &BTreeSet::from([id.to_string()]))
            .await
            .map_err(forge_err)?;
        for (org_id, team_id) in linked {
            let org = self
                .forge
                .orgs
                .org_any(cx, &org_id)
                .await
                .map_err(forge_err)?;
            self.teams
                .unlink_group(cx, &org, &team_id, "scim", id)
                .await
                .map_err(forge_err)?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await.map_err(forge_err)?;
        tx.delete(&SCIM_GROUPS, &key, id).await.map_err(forge_err)?;
        self.audit(
            &mut tx,
            scope,
            "enterprise.scim_group_delete",
            id,
            &json!({"display_name": g.display_name}),
            &(),
        )
        .await
        .map_err(forge_err)?;
        tx.commit().await.map_err(forge_err)?;
        Ok(ScimResponse {
            status: 204,
            body: Value::Null,
            location: None,
        })
    }
}

fn check_patch_schema(body: &Value) -> std::result::Result<(), ScimError> {
    let ok = body
        .get("schemas")
        .and_then(Value::as_array)
        .is_some_and(|s| s.iter().any(|x| x.as_str() == Some(SCHEMA_PATCH)));
    if ok {
        Ok(())
    } else {
        Err(ScimError::new(
            400,
            "invalidSyntax",
            "a PATCH body needs the PatchOp schema",
        ))
    }
}

fn service_provider_config() -> Value {
    json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
        "patch": {"supported": true},
        "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
        "filter": {"supported": true, "maxResults": MAX_COUNT},
        "changePassword": {"supported": false},
        "sort": {"supported": false},
        "etag": {"supported": false},
        "authenticationSchemes": [{
            "type": "oauthbearertoken", "name": "Bearer token",
            "description": "A SCIM token created in the org or enterprise settings",
            "primary": true
        }]
    })
}

fn resource_types() -> Value {
    json!({
        "schemas": [SCHEMA_LIST], "totalResults": 2, "startIndex": 1, "itemsPerPage": 2,
        "Resources": [
            {"schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"], "id": "User",
             "name": "User", "endpoint": "/Users", "schema": SCHEMA_USER},
            {"schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"], "id": "Group",
             "name": "Group", "endpoint": "/Groups", "schema": SCHEMA_GROUP}
        ]
    })
}

fn schemas() -> Value {
    json!({
        "schemas": [SCHEMA_LIST], "totalResults": 2, "startIndex": 1, "itemsPerPage": 2,
        "Resources": [
            {"id": SCHEMA_USER, "name": "User", "description": "User account",
             "attributes": [
                {"name": "userName", "type": "string", "multiValued": false, "required": true, "uniqueness": "server"},
                {"name": "externalId", "type": "string", "multiValued": false, "required": false},
                {"name": "displayName", "type": "string", "multiValued": false, "required": false},
                {"name": "active", "type": "boolean", "multiValued": false, "required": false},
                {"name": "emails", "type": "complex", "multiValued": true, "required": false}]},
            {"id": SCHEMA_GROUP, "name": "Group", "description": "Group of users",
             "attributes": [
                {"name": "displayName", "type": "string", "multiValued": false, "required": true},
                {"name": "members", "type": "complex", "multiValued": true, "required": false}]}
        ]
    })
}
