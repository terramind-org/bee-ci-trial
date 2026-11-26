//! Plumbing shared by every enterprise domain: paging over the row store, events and audit
//! helpers, secrets, and the small traits the domains use to call each other without
//! depending on each other's modules.
//!
//! Cross-domain traits:
//!
//! - [`ScopeResolver`]: resolves an `EnterpriseScope` (an org or an enterprise account) and
//!   authorizes a caller against it. [`OrgScopeResolver`] serves orgs only; the enterprise
//!   accounts module implements the full one.
//! - [`SpendGate`]: asks billing whether a charge may be incurred (`QUOTA_EXCEEDED`).
//! - [`IpPolicy`]: asks the IP allow lists whether an address may reach an org.
//! - [`TeamDirectory`]: asks teams who is in a team.
//! - [`TokenRevoker`]: revokes Biscuit token ids (the server owns the revocation list).

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    BillingProduct, EnterpriseScope, Event, Principal, PrincipalKind, Role, SubjectKind,
    SubjectRef, enterprise_scope,
};
use prost::Message;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::audit::AuditDraft;
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::store::{Cursor, Key, OutboxRecord, Row, Scan, TableDef, Tx};

/// Largest page any list returns.
pub const MAX_LIST: usize = 500;
/// Page size when the request names none.
pub const DEFAULT_LIST: usize = 50;
/// Upper bound for "read everything in a scope" scans.
pub const SCAN_CAP: usize = 10_000;

/// Cursor after the row with `id` in `scope` (ordering by id).
pub fn page_cursor(scope: &str, after: &str) -> Option<Cursor> {
    (!after.is_empty()).then(|| Cursor {
        n1: 0,
        scope: scope.to_string(),
        id: after.to_string(),
    })
}

/// Page size and token of a request.
pub fn page_args(p: &Option<bee_proto::v1::PageRequest>) -> (String, usize) {
    match p {
        Some(p) => (
            p.page_token.clone(),
            if p.page_size == 0 {
                DEFAULT_LIST
            } else {
                (p.page_size as usize).min(MAX_LIST)
            },
        ),
        None => (String::new(), DEFAULT_LIST),
    }
}

/// A `PageResponse` for a token (`None` when it is empty).
pub fn page_response(token: String) -> Option<bee_proto::v1::PageResponse> {
    Some(bee_proto::v1::PageResponse {
        next_page_token: token,
    })
}

/// Reads one page of a scope in id order. Returns the decoded rows and the token for the next
/// page (empty at the end).
pub async fn scan_page<M: Message + Default>(
    tx: &mut Tx,
    t: &TableDef,
    scope: &str,
    key: Option<Key>,
    after: &str,
    limit: usize,
) -> Result<(Vec<(M, Row)>, String)> {
    let limit = limit.clamp(1, MAX_LIST);
    let mut q = Scan::scope(scope, limit + 1).after(page_cursor(scope, after));
    if let Some(k) = key {
        q = q.key(k);
    }
    let rows = tx.scan_msgs::<M>(t, &q).await?;
    let more = rows.len() > limit;
    let rows: Vec<(M, Row)> = rows.into_iter().take(limit).collect();
    let next = if more {
        rows.last().map(|(_, r)| r.id.clone()).unwrap_or_default()
    } else {
        String::new()
    };
    Ok((rows, next))
}

/// Reads every row of a scope (bounded by [`SCAN_CAP`]).
pub async fn scan_all<M: Message + Default>(
    tx: &mut Tx,
    t: &TableDef,
    scope: &str,
    key: Option<Key>,
) -> Result<Vec<(M, Row)>> {
    let mut q = Scan::scope(scope, SCAN_CAP);
    if let Some(k) = key {
        q = q.key(k);
    }
    tx.scan_msgs::<M>(t, &q).await
}

/// Reads one row or answers `NOT_FOUND` naming `what`.
pub async fn get_or_404<M: Message + Default>(
    tx: &mut Tx,
    t: &TableDef,
    scope: &str,
    id: &str,
    what: &str,
) -> Result<(M, Row)> {
    tx.get_msg::<M>(t, scope, id)
        .await?
        .ok_or_else(|| ForgeError::not_found(format!("{what} {id}")))
}

/// Proto3-JSON of a message, for audit summaries and event payloads.
pub fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Records an event in `org_id`'s log. `subject` names the object; `payload` is its JSON.
pub async fn emit(
    tx: &mut Tx,
    kind: &str,
    org_id: &str,
    subject: Option<(SubjectKind, &str)>,
    payload: String,
) -> Result<()> {
    tx.emit_event(Event {
        kind: kind.into(),
        org_id: org_id.into(),
        payload_json: payload,
        subject: subject.map(|(k, id)| SubjectRef {
            kind: k as i32,
            repo: None,
            id: id.into(),
        }),
        ..Default::default()
    })
    .await
}

/// Records an audit entry in an enterprise account's chain (`enterprise:<id>`) instead of an
/// org's. The draft's org is ignored.
pub async fn audit_enterprise(tx: &mut Tx, enterprise_id: &str, draft: AuditDraft) -> Result<()> {
    let cx = tx.cx().clone();
    let mut entry = draft.into_entry(&cx, tx.now_ms(), tx.new_id()?);
    entry.org = String::new();
    entry.enterprise = enterprise_id.to_string();
    let id = tx.new_id()?;
    tx.raw()
        .outbox_push(
            &id,
            &OutboxRecord {
                event: None,
                audit: Some(entry),
            },
        )
        .await
}

/// Lower-case hex SHA-256, how every secret and token is stored.
pub fn sha256_hex(s: &str) -> String {
    crate::ids::hex(&Sha256::digest(s.as_bytes()))
}

/// Constant-time equality of two strings.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().min(b.len()) {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// A URL-safe slug of a display name: lower-case letters and digits separated by single `-`.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
    }
    out
}

/// Whether `s` is a valid slug: 1-100 characters of lower-case letters, digits, `-`, `_`, `.`,
/// not starting with `-` or `.`.
pub fn valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with(['-', '.'])
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

/// A principal reference carrying only an id and kind.
pub fn principal_ref(id: &str, kind: PrincipalKind) -> Principal {
    Principal {
        id: id.to_string(),
        kind: kind as i32,
        ..Default::default()
    }
}

/// The full principal for an id: from the directory when known, else a bare human reference.
pub async fn principal_of(forge: &Forge, id: &str) -> Principal {
    forge
        .hooks
        .directory
        .get(id)
        .await
        .unwrap_or_else(|| principal_ref(id, PrincipalKind::Human))
}

/// Whether a scope names an org or an enterprise account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    /// One org.
    Org,
    /// An enterprise account over several orgs.
    Enterprise,
}

/// A resolved `EnterpriseScope`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedScope {
    /// Org or enterprise.
    pub kind: ScopeKind,
    /// Org id or enterprise id.
    pub id: String,
    /// Org name or enterprise slug.
    pub name: String,
    /// The orgs the scope covers: the org itself, or every org of the enterprise.
    pub org_ids: Vec<String>,
    /// For an org: the enterprise it belongs to, if any.
    pub enterprise_id: Option<String>,
}

impl ResolvedScope {
    /// Stable partition key, `org:<id>` or `enterprise:<id>`.
    pub fn key(&self) -> String {
        match self.kind {
            ScopeKind::Org => format!("org:{}", self.id),
            ScopeKind::Enterprise => format!("enterprise:{}", self.id),
        }
    }

    /// The scope as the proto message (names, not ids).
    pub fn to_proto(&self) -> EnterpriseScope {
        EnterpriseScope {
            scope: Some(match self.kind {
                ScopeKind::Org => enterprise_scope::Scope::Org(self.name.clone()),
                ScopeKind::Enterprise => enterprise_scope::Scope::Enterprise(self.name.clone()),
            }),
        }
    }

    /// The org whose audit chain and event log record changes to this scope (the org itself;
    /// empty for an enterprise, which has its own chain).
    pub fn audit_org(&self) -> String {
        match self.kind {
            ScopeKind::Org => self.id.clone(),
            ScopeKind::Enterprise => String::new(),
        }
    }
}

/// How much access a call to a scope needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeNeed {
    /// Read settings (org members, enterprise owners).
    View,
    /// Change settings (org owners and admins, enterprise owners).
    Manage,
    /// Read or change billing (as Manage, plus billing managers).
    Billing,
}

/// Resolves scopes and authorizes callers against them.
#[async_trait]
pub trait ScopeResolver: Send + Sync {
    /// Resolves an `EnterpriseScope` (org name or id, enterprise slug or id).
    async fn resolve(&self, cx: &Cx, scope: &Option<EnterpriseScope>) -> Result<ResolvedScope>;
    /// Resolves an org name or id on its own.
    async fn resolve_org(&self, cx: &Cx, name_or_id: &str) -> Result<ResolvedScope>;
    /// Authorizes `caller` for `need` on `scope`; the returned context carries the decision.
    async fn authorize(
        &self,
        caller: &Caller,
        scope: &ResolvedScope,
        need: ScopeNeed,
    ) -> Result<Cx>;
}

/// Authorizes an org-level need: [`ScopeNeed::Billing`] additionally admits billing managers.
pub async fn authorize_org(
    forge: &Forge,
    caller: &Caller,
    org_id: &str,
    need: ScopeNeed,
) -> Result<Cx> {
    match need {
        ScopeNeed::View => forge.authz.org(caller, org_id, OrgPerm::ViewMembers).await,
        ScopeNeed::Manage => forge.authz.org(caller, org_id, OrgPerm::Manage).await,
        ScopeNeed::Billing => {
            if let Ok(cx) = forge.authz.org(caller, org_id, OrgPerm::Manage).await {
                return Ok(cx);
            }
            if !caller.token.allows_action(bee_policy::Action::Read) {
                return Err(ForgeError::permission_denied("token does not allow `read`"));
            }
            for id in &caller.cx.chain {
                let role = forge
                    .orgs
                    .member(&caller.cx, org_id, id)
                    .await?
                    .map(|m| Role::try_from(m.role).unwrap_or(Role::Member));
                if !matches!(role, Some(Role::Owner | Role::Admin | Role::BillingManager)) {
                    return Err(ForgeError::permission_denied(format!(
                        "{id} is not a billing manager of {org_id}"
                    )));
                }
            }
            let mut cx = caller.cx.clone();
            cx.decision = "token:ok billing_manager".into();
            Ok(cx)
        }
    }
}

/// A [`ScopeResolver`] that knows orgs only (no enterprise accounts).
pub struct OrgScopeResolver {
    forge: Arc<Forge>,
}

impl OrgScopeResolver {
    /// A resolver over `forge`.
    pub fn new(forge: Arc<Forge>) -> OrgScopeResolver {
        OrgScopeResolver { forge }
    }
}

#[async_trait]
impl ScopeResolver for OrgScopeResolver {
    async fn resolve(&self, cx: &Cx, scope: &Option<EnterpriseScope>) -> Result<ResolvedScope> {
        match scope.as_ref().and_then(|s| s.scope.as_ref()) {
            Some(enterprise_scope::Scope::Org(o)) => self.resolve_org(cx, o).await,
            Some(enterprise_scope::Scope::Enterprise(_)) => Err(ForgeError::invalid(
                "no_enterprises",
                "this deployment resolves orgs only",
            )),
            None => Err(ForgeError::missing("scope")),
        }
    }

    async fn resolve_org(&self, cx: &Cx, name_or_id: &str) -> Result<ResolvedScope> {
        let org = self.forge.orgs.get_org(cx, name_or_id).await?;
        Ok(ResolvedScope {
            kind: ScopeKind::Org,
            id: org.id.clone(),
            name: org.name,
            org_ids: vec![org.id],
            enterprise_id: None,
        })
    }

    async fn authorize(
        &self,
        caller: &Caller,
        scope: &ResolvedScope,
        need: ScopeNeed,
    ) -> Result<Cx> {
        match scope.kind {
            ScopeKind::Org => authorize_org(&self.forge, caller, &scope.id, need).await,
            ScopeKind::Enterprise => Err(ForgeError::permission_denied(
                "enterprise accounts are not served here",
            )),
        }
    }
}

/// Asks billing whether a charge may be incurred.
#[async_trait]
pub trait SpendGate: Send + Sync {
    /// `QUOTA_EXCEEDED` when a hard-stop budget covering this org, repo and product is spent
    /// or would be passed by `add_micro_usd`. `repo_id` may be empty.
    async fn check_spend(
        &self,
        cx: &Cx,
        org_id: &str,
        repo_id: &str,
        product: BillingProduct,
        add_micro_usd: u64,
    ) -> Result<()>;
}

/// Asks the IP allow lists whether an address may reach an org.
#[async_trait]
pub trait IpPolicy: Send + Sync {
    /// `IP_NOT_ALLOWED` when the org (or its enterprise) enforces an allow list that does not
    /// cover `ip`. `app_token` is true for installation and user-to-server tokens, which the
    /// list covers only when its settings say so.
    async fn check_ip(&self, cx: &Cx, org_id: &str, ip: &str, app_token: bool) -> Result<()>;
}

/// Asks teams who is in a team.
#[async_trait]
pub trait TeamDirectory: Send + Sync {
    /// Whether `principal_id` is a member of the team, directly or through a child team.
    async fn is_team_member(&self, cx: &Cx, team_id: &str, principal_id: &str) -> Result<bool>;
    /// The org a team belongs to (`NOT_FOUND` when the team does not exist).
    async fn team_org(&self, cx: &Cx, team_id: &str) -> Result<String>;
}

/// Revokes Biscuit tokens by id. The server owns the revocation list; tests use
/// [`bee_auth::InMemoryRevocations`].
#[async_trait]
pub trait TokenRevoker: Send + Sync {
    /// Revokes the token and every token derived from it.
    async fn revoke(&self, token_id: &str) -> Result<()>;
}

#[async_trait]
impl TokenRevoker for bee_auth::InMemoryRevocations {
    async fn revoke(&self, token_id: &str) -> Result<()> {
        bee_auth::InMemoryRevocations::revoke(self, &bee_auth::TokenId(token_id.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("Core Team (EU)!"), "core-team-eu");
        assert_eq!(slugify("---"), "");
        assert!(valid_slug("core-team"));
        assert!(!valid_slug("-x"));
        assert!(!valid_slug("Core"));
    }

    #[test]
    fn constant_time_equality() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert_eq!(sha256_hex("").len(), 64);
    }
}
