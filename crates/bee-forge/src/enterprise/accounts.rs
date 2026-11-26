//! Enterprise accounts above orgs: owners, enterprise policies, managed users, and the
//! per-scope settings store the identity and network controls use.
//!
//! An enterprise has a slug, owners (an enterprise always has one), a set of orgs and typed
//! [`EnterprisePolicies`]. Settings set at enterprise level override the same setting of its
//! orgs ([`Accounts::policies_of_org`]). [`Accounts`] also implements
//! [`ScopeResolver`], so billing and the identity settings treat an org and an enterprise
//! uniformly: an org's settings are managed by the org's owners and admins and by the owners
//! of the enterprise it belongs to; an enterprise's by its owners.
//!
//! Settings of identity and network controls live in one table, keyed by scope
//! (`org:<id>` / `enterprise:<id>`) and kind. A small [`Flags`] row summarizes which
//! controls are on, so admission ([`super::network`]) reads one row per scope on the hot path.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use bee_policy::Action;
use bee_proto::v1::{
    Enterprise, EnterpriseOrg, EnterpriseOwner, EnterprisePolicies, EnterpriseScope, Org,
    Principal, SubjectKind, enterprise_scope,
};
use prost::Message;

use super::SURFACE;
use super::common::{
    self, ResolvedScope, ScopeKind, ScopeNeed, ScopeResolver, audit_enterprise, authorize_org,
    get_or_404, principal_of, scan_all, scan_page,
};
use crate::audit::AuditDraft;
use crate::auth::Caller;
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
    ENTERPRISES,
    "forge_enterprises",
    true,
    [],
    "Enterprises (global scope); `k1` slug."
);
table!(
    ENTERPRISE_ORGS,
    "forge_enterprise_orgs",
    false,
    [K1],
    "Orgs of an enterprise, scoped by enterprise; id and `k1` the org id."
);
table!(
    ENTERPRISE_OWNERS,
    "forge_enterprise_owners",
    false,
    [K1],
    "Enterprise owners, scoped by enterprise; id and `k1` the principal id."
);
table!(
    ENTERPRISE_POLICIES,
    "forge_enterprise_policies",
    false,
    [],
    "Policies by enterprise id (global scope)."
);
table!(
    SCOPE_SETTINGS,
    "forge_scope_settings",
    false,
    [],
    "Identity and network settings; scope `org:<id>` or `enterprise:<id>`, id the kind."
);
table!(
    PRINCIPAL_STATES,
    "forge_principal_states",
    false,
    [],
    "Principals blocked from access (deprovisioned, suspended), by principal id."
);

/// Migration creating the account tables.
pub static M_ACCOUNTS: Migration = Migration {
    module: "enterprise",
    version: 3,
    name: "create_enterprise_accounts",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&ENTERPRISES),
        MigrationOp::CreateTable(&ENTERPRISE_ORGS),
        MigrationOp::CreateTable(&ENTERPRISE_OWNERS),
        MigrationOp::CreateTable(&ENTERPRISE_POLICIES),
        MigrationOp::CreateTable(&SCOPE_SETTINGS),
        MigrationOp::CreateTable(&PRINCIPAL_STATES),
    ],
};

/// Summary of the controls that are on for one scope.
#[derive(Clone, PartialEq, Message)]
pub struct Flags {
    /// An IP allow list is enforced.
    #[prost(bool, tag = "1")]
    pub ip: bool,
    /// The IP allow list also applies to installed apps' tokens.
    #[prost(bool, tag = "2")]
    pub ip_apps: bool,
    /// SAML SSO is enforced.
    #[prost(bool, tag = "3")]
    pub sso_enforced: bool,
    /// Two-factor authentication is required.
    #[prost(bool, tag = "4")]
    pub two_factor: bool,
    /// Only passkeys and security keys count as two-factor.
    #[prost(bool, tag = "5")]
    pub two_factor_secure_only: bool,
}

/// Why a principal is blocked from access.
#[derive(Clone, PartialEq, Message)]
pub struct PrincipalState {
    /// `deprovisioned` or `suspended`.
    #[prost(string, tag = "1")]
    pub state: String,
    /// Human-readable reason.
    #[prost(string, tag = "2")]
    pub reason: String,
    /// When the state was set (ms).
    #[prost(uint64, tag = "3")]
    pub since_ms: u64,
}

/// Settings kinds in [`SCOPE_SETTINGS`].
pub mod kind {
    /// `SamlSsoConfig`.
    pub const SAML: &str = "saml";
    /// `ScimConfig`.
    pub const SCIM: &str = "scim";
    /// `IpAllowListSettings`.
    pub const IP: &str = "ip";
    /// `VerifiedDomainSettings`.
    pub const DOMAINS: &str = "domains";
    /// `EnterpriseTwoFactorRequirement`.
    pub const TWO_FACTOR: &str = "two_factor";
    /// [`super::Flags`].
    pub const FLAGS: &str = "flags";
}

/// Reads a settings row of a scope, if set.
pub async fn setting<M: Message + Default>(
    tx: &mut Tx,
    scope_key: &str,
    kind: &str,
) -> Result<Option<(M, Row)>> {
    tx.get_msg::<M>(&SCOPE_SETTINGS, scope_key, kind).await
}

/// Writes a settings row of a scope.
pub async fn put_setting<M: Message>(
    tx: &mut Tx,
    scope_key: &str,
    kind: &str,
    value: &M,
) -> Result<()> {
    tx.put(&SCOPE_SETTINGS, &Row::new(scope_key, kind).msg(value))
        .await
        .map(|_| ())
}

/// Updates the flags row of a scope.
pub async fn update_flags(tx: &mut Tx, scope_key: &str, f: impl FnOnce(&mut Flags)) -> Result<()> {
    let mut flags = setting::<Flags>(tx, scope_key, kind::FLAGS)
        .await?
        .map(|(f, _)| f)
        .unwrap_or_default();
    f(&mut flags);
    put_setting(tx, scope_key, kind::FLAGS, &flags).await
}

/// A partial enterprise update.
#[derive(Clone, Debug, Default)]
pub struct EnterprisePatch {
    /// Name.
    pub name: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Website.
    pub website_url: Option<String>,
    /// Managed users mode.
    pub managed_users: Option<bool>,
    /// Suffix appended to managed usernames.
    pub managed_users_shortcode: Option<String>,
}

/// Enterprise accounts over the enterprise surface.
pub struct Accounts {
    db: Db,
    forge: Arc<Forge>,
}

impl Accounts {
    /// Accounts over the forge's store.
    pub fn new(forge: Arc<Forge>) -> Accounts {
        Accounts {
            db: forge.db.clone(),
            forge,
        }
    }

    async fn load(tx: &mut Tx, slug_or_id: &str) -> Result<(Enterprise, Row)> {
        if slug_or_id.is_empty() {
            return Err(ForgeError::missing("enterprise"));
        }
        if let Some(found) = tx
            .get_msg::<Enterprise>(&ENTERPRISES, "", slug_or_id)
            .await?
        {
            return Ok(found);
        }
        tx.scan_msgs::<Enterprise>(
            &ENTERPRISES,
            &Scan::scope("", 1).key(Key::K1(slug_or_id.to_lowercase())),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ForgeError::not_found(format!("enterprise {slug_or_id}")))
    }

    async fn org_ids_in(tx: &mut Tx, enterprise_id: &str) -> Result<Vec<String>> {
        Ok(tx
            .scan(
                &ENTERPRISE_ORGS,
                &Scan::scope(enterprise_id, crate::store::MAX_SCAN),
            )
            .await?
            .into_iter()
            .map(|r| r.id)
            .collect())
    }

    async fn fill(&self, cx: &Cx, tx: &mut Tx, mut e: Enterprise) -> Result<Enterprise> {
        let orgs = Self::org_ids_in(tx, &e.id).await?;
        e.org_count = orgs.len() as u32;
        let mut members = BTreeSet::new();
        for org in &orgs {
            let mut after = String::new();
            loop {
                let (page, next) = self.forge.orgs.members(cx, org, &after, 500).await?;
                for m in page {
                    if let Some(p) = m.principal {
                        members.insert(p.id);
                    }
                }
                if next.is_empty() {
                    break;
                }
                after = next;
            }
        }
        e.member_count = members.len() as u32;
        Ok(e)
    }

    /// Creates an enterprise account with its first owner. There is no RPC for this: site
    /// administrators (the Beehive operator) create enterprises (`docs/spec/orgs-apps.md`,
    /// API gaps).
    pub async fn create_enterprise(
        &self,
        cx: &Cx,
        slug: &str,
        name: &str,
        owner: Principal,
    ) -> Result<Enterprise> {
        if !common::valid_slug(slug) {
            return Err(ForgeError::invalid(
                "invalid_name",
                format!("`{slug}` is not a valid enterprise slug"),
            ));
        }
        if owner.id.is_empty() {
            return Err(ForgeError::missing("owner"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx
            .scan(
                &ENTERPRISES,
                &Scan::scope("", 1).key(Key::K1(slug.to_string())),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("enterprise {slug}")));
        }
        let now = tx.now_ms();
        let e = Enterprise {
            id: tx.new_id()?,
            slug: slug.into(),
            name: if name.is_empty() {
                slug.into()
            } else {
                name.into()
            },
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        tx.insert(
            &ENTERPRISES,
            &Row::new("", e.id.clone()).k1(slug.to_string()).msg(&e),
        )
        .await?;
        let o = EnterpriseOwner {
            principal: Some(owner.clone()),
            added_at: Some(ts(now)),
        };
        tx.insert(
            &ENTERPRISE_OWNERS,
            &Row::new(e.id.clone(), owner.id.clone())
                .k1(owner.id.clone())
                .msg(&o),
        )
        .await?;
        let pol = EnterprisePolicies {
            enterprise_id: e.id.clone(),
            updated_at: Some(ts(now)),
            ..default_policies()
        };
        tx.insert(&ENTERPRISE_POLICIES, &Row::new("", e.id.clone()).msg(&pol))
            .await?;
        let draft = AuditDraft::new("enterprise.create", "")
            .target_id(&e.id)
            .after(&common::json(&e));
        audit_enterprise(&mut tx, &e.id, draft).await?;
        tx.commit().await?;
        Ok(e)
    }

    /// Reads an enterprise by slug or id, with counts.
    pub async fn get(&self, cx: &Cx, enterprise: &str) -> Result<Enterprise> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, enterprise).await?;
        self.fill(cx, &mut tx, e).await
    }

    /// Changes an enterprise. Managed users can be turned on only while every member of its
    /// orgs is provisioned through SCIM.
    pub async fn update(
        &self,
        cx: &Cx,
        enterprise: &str,
        p: EnterprisePatch,
    ) -> Result<Enterprise> {
        if p.managed_users == Some(true) {
            let ent = self.get(cx, enterprise).await?;
            if !ent.managed_users {
                self.require_all_provisioned(cx, &ent.id).await?;
            }
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut e, row) = Self::load(&mut tx, enterprise).await?;
        let before = common::json(&e);
        if let Some(n) = p.name {
            e.name = n;
        }
        if let Some(d) = p.description {
            e.description = d;
        }
        if let Some(w) = p.website_url {
            e.website_url = w;
        }
        if let Some(m) = p.managed_users {
            e.managed_users = m;
        }
        if let Some(s) = p.managed_users_shortcode {
            if !s.is_empty() && !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(ForgeError::invalid(
                    "invalid_setting",
                    "the shortcode is letters, digits and `_`",
                ));
            }
            e.managed_users_shortcode = s;
        }
        e.updated_at = Some(ts(tx.now_ms()));
        tx.update(&ENTERPRISES, &row.msg(&e)).await?;
        let draft = AuditDraft::new("enterprise.update", "")
            .target_id(&e.id)
            .before(&before)
            .after(&common::json(&e));
        audit_enterprise(&mut tx, &e.id, draft).await?;
        let e = self.fill(cx, &mut tx, e).await?;
        tx.commit().await?;
        Ok(e)
    }

    async fn require_all_provisioned(&self, cx: &Cx, enterprise_id: &str) -> Result<()> {
        let org_ids = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::org_ids_in(&mut tx, enterprise_id).await?
        };
        let mut keys = vec![format!("enterprise:{enterprise_id}")];
        keys.extend(org_ids.iter().map(|o| format!("org:{o}")));
        let mut missing = Vec::new();
        for org in &org_ids {
            let mut after = String::new();
            loop {
                let (page, next) = self.forge.orgs.members(cx, org, &after, 500).await?;
                for m in page {
                    let Some(p) = m.principal else { continue };
                    let mut tx = self.db.begin(SURFACE, cx).await?;
                    if !super::scim::principal_provisioned(&mut tx, &keys, &p.id).await? {
                        missing.push(p.id);
                    }
                }
                if next.is_empty() {
                    break;
                }
                after = next;
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            missing.sort();
            missing.dedup();
            Err(ForgeError::invalid_state(format!(
                "managed users need every member provisioned through SCIM; not provisioned: {}",
                missing.join(", ")
            ))
            .with_subcode("members_not_provisioned"))
        }
    }

    /// The orgs of an enterprise with member and repo counts.
    pub async fn list_orgs(
        &self,
        cx: &Cx,
        enterprise: &str,
        query: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<EnterpriseOrg>, String)> {
        let (ent_id, rows, next) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let (e, _) = Self::load(&mut tx, enterprise).await?;
            let (rows, next) =
                scan_page::<EnterpriseOrg>(&mut tx, &ENTERPRISE_ORGS, &e.id, None, after, limit)
                    .await?;
            (e.id, rows, next)
        };
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for (mut eo, _) in rows {
            let Ok(org) = self.forge.orgs.org_any(cx, &eo.org_id).await else {
                continue;
            };
            if !q.is_empty() && !org.name.to_lowercase().contains(&q) {
                continue;
            }
            eo.enterprise_id = ent_id.clone();
            eo.org = org.name.clone();
            eo.member_count = self.forge.orgs.members(cx, &org.id, "", 500).await?.0.len() as u32;
            eo.repo_count = self
                .forge
                .orgs
                .list_repos(cx, &org.id, "", 500)
                .await?
                .0
                .len() as u32;
            out.push(eo);
        }
        Ok((out, next))
    }

    /// Adds an org to an enterprise (the orgs module creates new orgs; the caller passes the
    /// result).
    pub async fn add_org(&self, cx: &Cx, enterprise: &str, org: &Org) -> Result<EnterpriseOrg> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, enterprise).await?;
        let existing = tx
            .scan(
                &ENTERPRISE_ORGS,
                &Scan {
                    key: Some(Key::K1(org.id.clone())),
                    limit: 1,
                    ..Scan::default()
                },
            )
            .await?;
        if let Some(r) = existing.first() {
            return Err(if r.scope == e.id {
                ForgeError::already_exists(format!("org {} in {}", org.name, e.slug))
            } else {
                ForgeError::invalid_state(format!(
                    "{} already belongs to another enterprise",
                    org.name
                ))
            });
        }
        let eo = EnterpriseOrg {
            enterprise_id: e.id.clone(),
            org: org.name.clone(),
            org_id: org.id.clone(),
            member_count: 0,
            repo_count: 0,
            joined_at: Some(ts(tx.now_ms())),
        };
        tx.insert(
            &ENTERPRISE_ORGS,
            &Row::new(e.id.clone(), org.id.clone())
                .k1(org.id.clone())
                .msg(&eo),
        )
        .await?;
        common::emit(
            &mut tx,
            "enterprise.org_added",
            &org.id,
            Some((SubjectKind::Org, &org.id)),
            common::json(&eo),
        )
        .await?;
        let draft = AuditDraft::new("enterprise.org_add", "")
            .target(SubjectKind::Org, &org.id)
            .after(&common::json(&eo));
        audit_enterprise(&mut tx, &e.id, draft).await?;
        tx.audit(
            AuditDraft::new("enterprise.org_add", &org.id)
                .target(SubjectKind::Org, &org.id)
                .after(&serde_json::json!({"enterprise": e.slug}).to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(eo)
    }

    /// Removes an org from an enterprise; enterprise policies stop applying to it.
    pub async fn remove_org(&self, cx: &Cx, enterprise: &str, org: &Org) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, enterprise).await?;
        if !tx.delete(&ENTERPRISE_ORGS, &e.id, &org.id).await? {
            return Err(ForgeError::not_found(format!(
                "org {} in {}",
                org.name, e.slug
            )));
        }
        common::emit(
            &mut tx,
            "enterprise.org_removed",
            &org.id,
            Some((SubjectKind::Org, &org.id)),
            serde_json::json!({"enterprise": e.slug, "org": org.name}).to_string(),
        )
        .await?;
        let draft = AuditDraft::new("enterprise.org_remove", "").target(SubjectKind::Org, &org.id);
        audit_enterprise(&mut tx, &e.id, draft).await?;
        tx.audit(
            AuditDraft::new("enterprise.org_remove", &org.id)
                .target(SubjectKind::Org, &org.id)
                .after(&serde_json::json!({"enterprise": e.slug}).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// The enterprise an org belongs to.
    pub async fn enterprise_of_org(&self, cx: &Cx, org_id: &str) -> Result<Option<Enterprise>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::enterprise_of_org_in(&mut tx, org_id).await
    }

    /// As [`Accounts::enterprise_of_org`], inside a transaction.
    pub async fn enterprise_of_org_in(tx: &mut Tx, org_id: &str) -> Result<Option<Enterprise>> {
        let rows = tx
            .scan(
                &ENTERPRISE_ORGS,
                &Scan {
                    key: Some(Key::K1(org_id.to_string())),
                    limit: 1,
                    ..Scan::default()
                },
            )
            .await?;
        match rows.first() {
            Some(r) => Ok(tx
                .get_msg::<Enterprise>(&ENTERPRISES, "", &r.scope)
                .await?
                .map(|(e, _)| e)),
            None => Ok(None),
        }
    }

    /// Lists enterprise owners.
    pub async fn owners(
        &self,
        cx: &Cx,
        enterprise: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<EnterpriseOwner>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, enterprise).await?;
        let (rows, next) =
            scan_page::<EnterpriseOwner>(&mut tx, &ENTERPRISE_OWNERS, &e.id, None, after, limit)
                .await?;
        Ok((rows.into_iter().map(|(o, _)| o).collect(), next))
    }

    /// Whether `principal_id` owns the enterprise.
    pub async fn is_owner(&self, cx: &Cx, enterprise_id: &str, principal_id: &str) -> Result<bool> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get(&ENTERPRISE_OWNERS, enterprise_id, principal_id)
            .await?
            .is_some())
    }

    /// Makes a principal an enterprise owner.
    pub async fn add_owner(
        &self,
        cx: &Cx,
        enterprise: &str,
        principal_id: &str,
    ) -> Result<EnterpriseOwner> {
        if principal_id.is_empty() {
            return Err(ForgeError::missing("principal_id"));
        }
        let p = principal_of(&self.forge, principal_id).await;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, enterprise).await?;
        if tx
            .get(&ENTERPRISE_OWNERS, &e.id, principal_id)
            .await?
            .is_some()
        {
            return Err(ForgeError::already_exists(format!("owner {principal_id}")));
        }
        let o = EnterpriseOwner {
            principal: Some(p),
            added_at: Some(ts(tx.now_ms())),
        };
        tx.insert(
            &ENTERPRISE_OWNERS,
            &Row::new(e.id.clone(), principal_id.to_string())
                .k1(principal_id.to_string())
                .msg(&o),
        )
        .await?;
        let draft =
            AuditDraft::new("enterprise.owner_add", "").target(SubjectKind::User, principal_id);
        audit_enterprise(&mut tx, &e.id, draft).await?;
        tx.commit().await?;
        Ok(o)
    }

    /// Removes an enterprise owner. The last owner cannot be removed.
    pub async fn remove_owner(&self, cx: &Cx, enterprise: &str, principal_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, enterprise).await?;
        // Serialize owner removals on the enterprise row before counting owners. The row
        // exists for every enterprise, and the lock is held through the delete and audit.
        tx.get_for_update(&ENTERPRISES, "", &e.id).await?;
        if tx
            .get(&ENTERPRISE_OWNERS, &e.id, principal_id)
            .await?
            .is_none()
        {
            return Err(ForgeError::not_found(format!("owner {principal_id}")));
        }
        if tx.count(&ENTERPRISE_OWNERS, &e.id, None).await? <= 1 {
            return Err(ForgeError::invalid_state(
                "an enterprise needs at least one owner",
            ));
        }
        tx.delete(&ENTERPRISE_OWNERS, &e.id, principal_id).await?;
        let draft =
            AuditDraft::new("enterprise.owner_remove", "").target(SubjectKind::User, principal_id);
        audit_enterprise(&mut tx, &e.id, draft).await?;
        tx.commit().await
    }

    /// The policies of an enterprise.
    pub async fn policies(&self, cx: &Cx, enterprise: &str) -> Result<EnterprisePolicies> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, enterprise).await?;
        Self::policies_in(&mut tx, &e.id).await
    }

    async fn policies_in(tx: &mut Tx, enterprise_id: &str) -> Result<EnterprisePolicies> {
        Ok(tx
            .get_msg::<EnterprisePolicies>(&ENTERPRISE_POLICIES, "", enterprise_id)
            .await?
            .map(|(p, _)| p)
            .unwrap_or(EnterprisePolicies {
                enterprise_id: enterprise_id.to_string(),
                ..default_policies()
            }))
    }

    /// Replaces the policies with `f`'s result and audits the change.
    pub async fn update_policies(
        &self,
        cx: &Cx,
        enterprise: &str,
        f: impl FnOnce(&mut EnterprisePolicies),
    ) -> Result<EnterprisePolicies> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, enterprise).await?;
        let before = Self::policies_in(&mut tx, &e.id).await?;
        let mut p = before.clone();
        f(&mut p);
        p.enterprise_id = e.id.clone();
        p.updated_at = Some(ts(tx.now_ms()));
        tx.put(&ENTERPRISE_POLICIES, &Row::new("", e.id.clone()).msg(&p))
            .await?;
        let draft = AuditDraft::new("enterprise.policies_update", "")
            .target_id(&e.id)
            .before(&common::json(&before))
            .after(&common::json(&p));
        audit_enterprise(&mut tx, &e.id, draft).await?;
        tx.commit().await?;
        Ok(p)
    }

    /// The enterprise policies binding an org, if it belongs to an enterprise.
    pub async fn policies_of_org(
        &self,
        cx: &Cx,
        org_id: &str,
    ) -> Result<Option<EnterprisePolicies>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        match Self::enterprise_of_org_in(&mut tx, org_id).await? {
            Some(e) => Ok(Some(Self::policies_in(&mut tx, &e.id).await?)),
            None => Ok(None),
        }
    }

    /// Managed-user rule: a managed principal (provisioned by an enterprise with managed users
    /// on) may join only orgs of its enterprise. The server calls this when a principal
    /// accepts an invitation or is added to an org.
    pub async fn check_can_join(&self, cx: &Cx, org_id: &str, principal_id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let ents = scan_all::<Enterprise>(&mut tx, &ENTERPRISES, "", None).await?;
        let target = Self::enterprise_of_org_in(&mut tx, org_id)
            .await?
            .map(|e| e.id);
        for (e, _) in ents {
            if !e.managed_users {
                continue;
            }
            let key = format!("enterprise:{}", e.id);
            if super::scim::principal_provisioned(&mut tx, std::slice::from_ref(&key), principal_id)
                .await?
                && target.as_deref() != Some(e.id.as_str())
            {
                return Err(ForgeError::permission_denied(format!(
                    "{principal_id} is a managed user of {} and cannot join orgs outside it",
                    e.slug
                ))
                .with_subcode("managed_user"));
            }
        }
        Ok(())
    }

    /// Blocks a principal from all access (`state` = `deprovisioned` or `suspended`).
    pub async fn set_principal_state(
        &self,
        cx: &Cx,
        principal_id: &str,
        state: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        match state {
            Some((s, reason)) => {
                let rec = PrincipalState {
                    state: s.into(),
                    reason: reason.into(),
                    since_ms: tx.now_ms(),
                };
                tx.put(&PRINCIPAL_STATES, &Row::new("", principal_id).msg(&rec))
                    .await?;
            }
            None => {
                tx.delete(&PRINCIPAL_STATES, "", principal_id).await?;
            }
        }
        tx.commit().await
    }

    /// The access block on a principal, if any.
    pub async fn principal_state(
        &self,
        cx: &Cx,
        principal_id: &str,
    ) -> Result<Option<PrincipalState>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(tx
            .get_msg::<PrincipalState>(&PRINCIPAL_STATES, "", principal_id)
            .await?
            .map(|(s, _)| s))
    }

    async fn resolve_enterprise(&self, cx: &Cx, name: &str) -> Result<ResolvedScope> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) = Self::load(&mut tx, name).await?;
        let org_ids = Self::org_ids_in(&mut tx, &e.id).await?;
        Ok(ResolvedScope {
            kind: ScopeKind::Enterprise,
            id: e.id,
            name: e.slug,
            org_ids,
            enterprise_id: None,
        })
    }

    /// Fails unless every principal of the chain owns the enterprise and the token may manage.
    async fn authorize_enterprise(
        &self,
        caller: &Caller,
        enterprise_id: &str,
        need: ScopeNeed,
    ) -> Result<Cx> {
        if matches!(need, ScopeNeed::Manage | ScopeNeed::Billing) {
            if caller.token.scope.repos.is_some() {
                return Err(ForgeError::policy_denied(
                    "token_scope",
                    vec![],
                    "enterprise management needs a token not restricted to repos",
                ));
            }
            if !caller.token.allows_action(Action::ManageSettings) {
                return Err(ForgeError::policy_denied(
                    "token_scope",
                    vec!["manage_settings".into()],
                    "token does not allow `manage_settings`",
                ));
            }
        } else if !caller.token.allows_action(Action::Read) {
            return Err(ForgeError::policy_denied(
                "token_scope",
                vec!["read".into()],
                "token does not allow `read`",
            ));
        }
        for id in &caller.cx.chain {
            if !self.is_owner(&caller.cx, enterprise_id, id).await? {
                return Err(ForgeError::permission_denied(format!(
                    "{id} is not an owner of the enterprise"
                )));
            }
        }
        let mut cx = caller.cx.clone();
        cx.decision = format!("token:ok enterprise_owner:{enterprise_id}");
        Ok(cx)
    }
}

/// Policies of a new enterprise: every choice left to the orgs.
pub fn default_policies() -> EnterprisePolicies {
    use bee_proto::v1::{
        EnterpriseActionsEnablement, EnterpriseActionsPolicy, EnterpriseAllowedActions,
        EnterpriseBasePermission, EnterpriseForkingPolicy, EnterprisePolicyToggle,
        EnterpriseRepoCreationPolicy,
    };
    EnterprisePolicies {
        repo_creation: EnterpriseRepoCreationPolicy::NoPolicy as i32,
        forking: EnterpriseForkingPolicy::NoPolicy as i32,
        base_permission: EnterpriseBasePermission::NoPolicy as i32,
        members_can_delete_repos: EnterprisePolicyToggle::NoPolicy as i32,
        members_can_change_repo_visibility: EnterprisePolicyToggle::NoPolicy as i32,
        members_can_invite_outside_collaborators: EnterprisePolicyToggle::NoPolicy as i32,
        members_can_create_teams: EnterprisePolicyToggle::NoPolicy as i32,
        members_can_install_apps: EnterprisePolicyToggle::NoPolicy as i32,
        agents_can_land_without_human_approval: EnterprisePolicyToggle::NoPolicy as i32,
        actions: Some(EnterpriseActionsPolicy {
            enablement: EnterpriseActionsEnablement::NoPolicy as i32,
            allowed_actions: EnterpriseAllowedActions::NoPolicy as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[async_trait]
impl ScopeResolver for Accounts {
    async fn resolve(&self, cx: &Cx, scope: &Option<EnterpriseScope>) -> Result<ResolvedScope> {
        match scope.as_ref().and_then(|s| s.scope.as_ref()) {
            Some(enterprise_scope::Scope::Org(o)) => self.resolve_org(cx, o).await,
            Some(enterprise_scope::Scope::Enterprise(e)) => self.resolve_enterprise(cx, e).await,
            None => Err(ForgeError::missing("scope")),
        }
    }

    async fn resolve_org(&self, cx: &Cx, name_or_id: &str) -> Result<ResolvedScope> {
        let org = self.forge.orgs.get_org(cx, name_or_id).await?;
        let enterprise_id = self.enterprise_of_org(cx, &org.id).await?.map(|e| e.id);
        Ok(ResolvedScope {
            kind: ScopeKind::Org,
            id: org.id.clone(),
            name: org.name,
            org_ids: vec![org.id],
            enterprise_id,
        })
    }

    async fn authorize(
        &self,
        caller: &Caller,
        scope: &ResolvedScope,
        need: ScopeNeed,
    ) -> Result<Cx> {
        match scope.kind {
            ScopeKind::Enterprise => self.authorize_enterprise(caller, &scope.id, need).await,
            ScopeKind::Org => match authorize_org(&self.forge, caller, &scope.id, need).await {
                Ok(cx) => Ok(cx),
                Err(e) => match &scope.enterprise_id {
                    Some(ent)
                        if matches!(e.reason, bee_proto::v1::ErrorReason::PermissionDenied) =>
                    {
                        self.authorize_enterprise(caller, ent, need)
                            .await
                            .map_err(|_| e)
                    }
                    _ => Err(e),
                },
            },
        }
    }
}

/// Fetches an enterprise row by id inside a transaction (shared by SCIM and SSO).
pub async fn enterprise_by_id(tx: &mut Tx, id: &str) -> Result<Enterprise> {
    Ok(
        get_or_404::<Enterprise>(tx, &ENTERPRISES, "", id, "enterprise")
            .await?
            .0,
    )
}
