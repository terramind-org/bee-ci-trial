//! Network and account-security controls of an org or enterprise: IP allow lists, verified
//! and approved domains, required two-factor authentication, and the admission check that
//! enforces them together with SSO sessions and access blocks.
//!
//! # Admission
//!
//! The server calls [`Network::admit`] for every authenticated request, before any handler,
//! with the principals of the call (actor and delegation chain), the orgs the request touches,
//! the source address and whether the token belongs to an installed app. It fails with:
//!
//! | reason | when |
//! |---|---|
//! | `PERMISSION_DENIED` (`principal_blocked`) | a principal of the call is deprovisioned or suspended |
//! | `IP_NOT_ALLOWED` | an allow list is enforced for the org (or its enterprise, which overrides the org) and the address is not on it |
//! | `SSO_REQUIRED` | SAML is enforced and an org member of the call has no live SSO session |
//! | `TWO_FACTOR_REQUIRED` | two-factor is required and an org member of the call has none |
//!
//! Each scope keeps a one-row summary of its enforced controls ([`super::accounts::Flags`]),
//! so admission for an org with nothing enforced costs two point reads on the enterprise
//! surface and one membership lookup per principal.
//!
//! # Domains
//!
//! A domain is claimed with a DNS TXT challenge at
//! `_beehive-challenge-<scope>.<domain>` whose value is `beehive-challenge=<token>`; checking
//! resolves it through a [`DnsResolver`] (the server supplies a real resolver). A domain is
//! verified for one scope at a time. "Approved" domains skip the check and count only for
//! notification restriction.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_proto::v1::{
    EnterpriseTwoFactorMember, EnterpriseTwoFactorRequirement, IpAllowListEntry,
    IpAllowListSettings, VerifiedDomain, VerifiedDomainSettings, VerifiedDomainState,
};

use super::SURFACE;
use super::accounts::{Accounts, Flags, kind, put_setting, setting, update_flags};
use super::common::{
    self, IpPolicy, ResolvedScope, ScopeKind, audit_enterprise, get_or_404, scan_all, scan_page,
};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::ts;
use crate::store::{Db, Index, Key, Migration, MigrationOp, Phase, Row, Scan, TableDef, Tx};

/// IP allow list entries, scoped by scope key.
pub static IP_ENTRIES: TableDef = TableDef {
    name: "forge_ip_allow_entries",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};

/// Domains, scoped by scope key; `k1` lower-case domain (unique per scope, indexed across scopes).
pub static DOMAINS: TableDef = TableDef {
    name: "forge_verified_domains",
    surface: SURFACE,
    unique_k1: true,
    unique_k2: false,
    indexes: &[Index::K1],
};

/// Migration creating the network tables.
pub static M_NETWORK: Migration = Migration {
    module: "enterprise",
    version: 4,
    name: "create_network_controls",
    phase: Phase::Expand,
    ops: &[
        MigrationOp::CreateTable(&IP_ENTRIES),
        MigrationOp::CreateTable(&DOMAINS),
    ],
};

/// Days a domain challenge stays valid.
pub const CHALLENGE_DAYS: u64 = 7;
/// Most IP allow list entries per scope.
pub const MAX_IP_ENTRIES: u64 = 1000;
const DAY_MS: u64 = 86_400_000;

/// An IPv4 or IPv6 range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

impl Cidr {
    /// Parses `a.b.c.d/n`, `x::y/n` or a bare address (a single-host range). Host bits are
    /// cleared.
    pub fn parse(s: &str) -> Result<Cidr> {
        let bad = || {
            ForgeError::invalid(
                "invalid_cidr",
                format!("`{s}` is not an IPv4 or IPv6 range"),
            )
        };
        let (a, p) = match s.trim().split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s.trim(), None),
        };
        let parsed = a.parse::<IpAddr>().map_err(|_| bad())?;
        let mapped = matches!(parsed, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some());
        let addr = unmap(parsed);
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => {
                let n = p.parse::<u8>().map_err(|_| bad())?;
                if mapped && n >= 96 { n - 96 } else { n }
            }
            None => max,
        };
        if prefix > max {
            return Err(bad());
        }
        Ok(Cidr {
            addr: mask(addr, prefix),
            prefix,
        })
    }

    /// Whether the range contains `ip`.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = unmap(ip);
        ip.is_ipv4() == self.addr.is_ipv4() && mask(ip, self.prefix) == self.addr
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let m = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix))
            };
            IpAddr::V4((u32::from(v4) & m).into())
        }
        IpAddr::V6(v6) => {
            let m = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            IpAddr::V6((u128::from(v6) & m).into())
        }
    }
}

/// Resolves DNS TXT records.
#[async_trait]
pub trait DnsResolver: Send + Sync {
    /// The TXT strings at `name`; empty when the name has none (or does not exist).
    async fn txt(&self, name: &str) -> Result<Vec<String>>;
}

/// A resolver over a fixed table (tests, air-gapped installs, and the server's override).
#[derive(Default)]
pub struct StaticResolver {
    records: Mutex<BTreeMap<String, Vec<String>>>,
}

impl StaticResolver {
    /// An empty table.
    pub fn new() -> StaticResolver {
        StaticResolver::default()
    }

    /// Publishes a TXT record.
    pub fn publish(&self, name: &str, value: &str) {
        if let Ok(mut m) = self.records.lock() {
            m.entry(name.to_lowercase())
                .or_default()
                .push(value.to_string());
        }
    }

    /// Removes every record at a name.
    pub fn clear(&self, name: &str) {
        if let Ok(mut m) = self.records.lock() {
            m.remove(&name.to_lowercase());
        }
    }
}

#[async_trait]
impl DnsResolver for StaticResolver {
    async fn txt(&self, name: &str) -> Result<Vec<String>> {
        Ok(self
            .records
            .lock()
            .map_err(|_| ForgeError::internal("resolver poisoned"))?
            .get(&name.to_lowercase())
            .cloned()
            .unwrap_or_default())
    }
}

/// Knows whether principals have two-factor authentication (the user service owns it).
#[async_trait]
pub trait TwoFactorDirectory: Send + Sync {
    /// Whether the principal has any second factor.
    async fn has_two_factor(&self, principal_id: &str) -> bool;
    /// Whether the principal has a passkey or security key.
    async fn has_secure_two_factor(&self, principal_id: &str) -> bool;
}

/// An in-memory two-factor directory.
#[derive(Default)]
pub struct MemoryTwoFactor {
    inner: Mutex<BTreeMap<String, bool>>,
}

impl MemoryTwoFactor {
    /// An empty directory (nobody has two-factor).
    pub fn new() -> MemoryTwoFactor {
        MemoryTwoFactor::default()
    }

    /// Records that a principal has a second factor (`secure` = a passkey or security key).
    pub fn enable(&self, principal_id: &str, secure: bool) {
        if let Ok(mut m) = self.inner.lock() {
            m.insert(principal_id.to_string(), secure);
        }
    }
}

#[async_trait]
impl TwoFactorDirectory for MemoryTwoFactor {
    async fn has_two_factor(&self, principal_id: &str) -> bool {
        self.inner
            .lock()
            .is_ok_and(|m| m.contains_key(principal_id))
    }

    async fn has_secure_two_factor(&self, principal_id: &str) -> bool {
        self.inner
            .lock()
            .is_ok_and(|m| m.get(principal_id).copied().unwrap_or(false))
    }
}

/// What admission needs to know about a request.
#[derive(Clone, Debug, Default)]
pub struct AdmissionRequest {
    /// The principals of the call: actor first, then delegators.
    pub principals: Vec<String>,
    /// The orgs the request touches (a repo request: the repo's org).
    pub org_ids: Vec<String>,
    /// Source address as text (empty = unknown, which no allow list admits).
    pub ip: String,
    /// The token belongs to an installed app (installation or user-to-server token).
    pub app_token: bool,
}

/// A partial IP allow list settings update.
#[derive(Clone, Copy, Debug, Default)]
pub struct IpSettingsPatch {
    /// Enforce the list.
    pub enabled: Option<bool>,
    /// Add the ranges installed apps declared.
    pub include_installed_app_ranges: Option<bool>,
    /// Apply the list to app tokens.
    pub applies_to_installed_apps: Option<bool>,
}

/// IP allow lists, domains, 2FA and admission over the enterprise surface.
pub struct Network {
    db: Db,
    forge: Arc<Forge>,
    accounts: Arc<Accounts>,
    dns: Arc<dyn DnsResolver>,
    two_factor: Arc<dyn TwoFactorDirectory>,
}

impl Network {
    /// Controls over the forge's store.
    pub fn new(
        forge: Arc<Forge>,
        accounts: Arc<Accounts>,
        dns: Arc<dyn DnsResolver>,
        two_factor: Arc<dyn TwoFactorDirectory>,
    ) -> Network {
        Network {
            db: forge.db.clone(),
            forge,
            accounts,
            dns,
            two_factor,
        }
    }

    // ---------------------------------------------------------------- IP allow lists

    async fn ip_settings_in(tx: &mut Tx, key: &str) -> Result<IpAllowListSettings> {
        Ok(setting::<IpAllowListSettings>(tx, key, kind::IP)
            .await?
            .map(|(s, _)| s)
            .unwrap_or_default())
    }

    /// The IP allow list settings of a scope.
    pub async fn ip_settings(&self, cx: &Cx, scope: &ResolvedScope) -> Result<IpAllowListSettings> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut s = Self::ip_settings_in(&mut tx, &scope.key()).await?;
        s.scope = Some(scope.to_proto());
        Ok(s)
    }

    async fn active_ranges(tx: &mut Tx, key: &str, include_apps: bool) -> Result<Vec<Cidr>> {
        let mut out = Vec::new();
        for (e, _) in scan_all::<IpAllowListEntry>(tx, &IP_ENTRIES, key, None).await? {
            if !e.active || (!e.app_id.is_empty() && !include_apps) {
                continue;
            }
            out.push(Cidr::parse(&e.cidr)?);
        }
        Ok(out)
    }

    fn lock_out(cx: &Cx, ranges: &[Cidr]) -> Result<()> {
        let Ok(ip) = cx.ip.parse::<IpAddr>() else {
            return Ok(());
        };
        if ranges.iter().any(|r| r.contains(ip)) {
            Ok(())
        } else {
            Err(ForgeError::invalid(
                "would_lock_out",
                format!("your address {ip} would not be allowed by the list"),
            ))
        }
    }

    /// Changes IP allow list enforcement. Enabling is refused (`would_lock_out`) when the
    /// caller's own address would not be allowed.
    pub async fn update_ip_settings(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        p: IpSettingsPatch,
    ) -> Result<IpAllowListSettings> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let before = Self::ip_settings_in(&mut tx, &key).await?;
        let mut s = before.clone();
        if let Some(v) = p.enabled {
            s.enabled = v;
        }
        if let Some(v) = p.include_installed_app_ranges {
            s.include_installed_app_ranges = v;
        }
        if let Some(v) = p.applies_to_installed_apps {
            s.applies_to_installed_apps = v;
        }
        if s.enabled {
            let ranges = Self::active_ranges(&mut tx, &key, s.include_installed_app_ranges).await?;
            Self::lock_out(cx, &ranges)?;
        }
        s.updated_at = Some(ts(tx.now_ms()));
        put_setting(&mut tx, &key, kind::IP, &s).await?;
        let (enabled, apps) = (s.enabled, s.applies_to_installed_apps);
        update_flags(&mut tx, &key, |f| {
            f.ip = enabled;
            f.ip_apps = apps;
        })
        .await?;
        self.record(
            &mut tx,
            scope,
            "enterprise.ip_settings_update",
            &key,
            &before,
            &s,
        )
        .await?;
        tx.commit().await?;
        s.scope = Some(scope.to_proto());
        Ok(s)
    }

    async fn record<B: serde::Serialize, A: serde::Serialize>(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
        action: &str,
        target: &str,
        before: &B,
        after: &A,
    ) -> Result<()> {
        let draft = AuditDraft::new(action, &scope.audit_org())
            .target_id(target)
            .before(&common::json(before))
            .after(&common::json(after));
        match scope.kind {
            ScopeKind::Org => tx.audit(draft).await,
            ScopeKind::Enterprise => audit_enterprise(tx, &scope.id, draft).await,
        }
    }

    /// Lists entries.
    pub async fn ip_entries(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<IpAllowListEntry>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) =
            scan_page::<IpAllowListEntry>(&mut tx, &IP_ENTRIES, &scope.key(), None, after, limit)
                .await?;
        Ok((
            rows.into_iter()
                .map(|(mut e, _)| {
                    e.scope = Some(scope.to_proto());
                    e
                })
                .collect(),
            next,
        ))
    }

    /// Adds an entry.
    pub async fn create_ip_entry(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        cidr: &str,
        description: &str,
        active: bool,
    ) -> Result<IpAllowListEntry> {
        let cidr = Cidr::parse(cidr)?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if tx.count(&IP_ENTRIES, &key, None).await? >= MAX_IP_ENTRIES {
            return Err(ForgeError::too_large(
                "ip_allow_entries",
                MAX_IP_ENTRIES,
                MAX_IP_ENTRIES + 1,
            ));
        }
        let now = tx.now_ms();
        let e = IpAllowListEntry {
            id: tx.new_id()?,
            scope: Some(scope.to_proto()),
            cidr: cidr.to_string(),
            description: description.into(),
            active,
            app_id: String::new(),
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
        };
        tx.insert(&IP_ENTRIES, &Row::new(key.clone(), e.id.clone()).msg(&e))
            .await?;
        self.record(&mut tx, scope, "enterprise.ip_entry_create", &e.id, &(), &e)
            .await?;
        tx.commit().await?;
        Ok(e)
    }

    /// Changes an entry; refused (`would_lock_out`) when enforcement is on and the caller
    /// would lose access.
    pub async fn update_ip_entry(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        cidr: Option<&str>,
        description: Option<&str>,
        active: Option<bool>,
    ) -> Result<IpAllowListEntry> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut e, row) =
            get_or_404::<IpAllowListEntry>(&mut tx, &IP_ENTRIES, &key, id, "ip allow list entry")
                .await?;
        if !e.app_id.is_empty() {
            return Err(ForgeError::invalid_state(
                "entries declared by apps are read only",
            ));
        }
        let before = e.clone();
        if let Some(c) = cidr {
            e.cidr = Cidr::parse(c)?.to_string();
        }
        if let Some(d) = description {
            e.description = d.into();
        }
        if let Some(a) = active {
            e.active = a;
        }
        e.updated_at = Some(ts(tx.now_ms()));
        tx.update(&IP_ENTRIES, &row.msg(&e)).await?;
        let s = Self::ip_settings_in(&mut tx, &key).await?;
        if s.enabled {
            let ranges = Self::active_ranges(&mut tx, &key, s.include_installed_app_ranges).await?;
            Self::lock_out(cx, &ranges)?;
        }
        self.record(
            &mut tx,
            scope,
            "enterprise.ip_entry_update",
            id,
            &before,
            &e,
        )
        .await?;
        tx.commit().await?;
        e.scope = Some(scope.to_proto());
        Ok(e)
    }

    /// Deletes an entry (with the same lock-out guard).
    pub async fn delete_ip_entry(&self, cx: &Cx, scope: &ResolvedScope, id: &str) -> Result<()> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (e, _) =
            get_or_404::<IpAllowListEntry>(&mut tx, &IP_ENTRIES, &key, id, "ip allow list entry")
                .await?;
        if !e.app_id.is_empty() {
            return Err(ForgeError::invalid_state(
                "entries declared by apps are read only",
            ));
        }
        tx.delete(&IP_ENTRIES, &key, id).await?;
        let s = Self::ip_settings_in(&mut tx, &key).await?;
        if s.enabled {
            let ranges = Self::active_ranges(&mut tx, &key, s.include_installed_app_ranges).await?;
            Self::lock_out(cx, &ranges)?;
        }
        self.record(&mut tx, scope, "enterprise.ip_entry_delete", id, &e, &())
            .await?;
        tx.commit().await
    }

    /// Replaces the ranges an installed app declares for a scope (the apps module calls this
    /// when an installation is created, changed or removed; an empty list removes them).
    pub async fn set_app_ranges(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        app_id: &str,
        cidrs: &[String],
    ) -> Result<()> {
        let parsed = cidrs
            .iter()
            .map(|c| Cidr::parse(c))
            .collect::<Result<Vec<_>>>()?;
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        tx.add_n1(
            &super::accounts::SCOPE_SETTINGS,
            &key,
            &format!("ip_ranges:{app_id}"),
            0,
        )
        .await?;
        for (e, row) in scan_all::<IpAllowListEntry>(&mut tx, &IP_ENTRIES, &key, None).await? {
            if e.app_id == app_id {
                tx.delete(&IP_ENTRIES, &key, &row.id).await?;
            }
        }
        let now = tx.now_ms();
        for c in parsed {
            let e = IpAllowListEntry {
                id: tx.new_id()?,
                scope: Some(scope.to_proto()),
                cidr: c.to_string(),
                description: format!("declared by app {app_id}"),
                active: true,
                app_id: app_id.into(),
                created_at: Some(ts(now)),
                updated_at: Some(ts(now)),
            };
            tx.insert(&IP_ENTRIES, &Row::new(key.clone(), e.id.clone()).msg(&e))
                .await?;
        }
        self.record(
            &mut tx,
            scope,
            "enterprise.ip_app_ranges_set",
            app_id,
            &(),
            &serde_json::json!({"app": app_id, "cidrs": cidrs}),
        )
        .await?;
        tx.commit().await
    }

    /// The rule in force for an org: the enterprise list when its enterprise enforces one,
    /// else the org's own. `None` when nothing is enforced.
    async fn ip_rule(tx: &mut Tx, org_id: &str) -> Result<Option<(Vec<Cidr>, bool)>> {
        let ent = Accounts::enterprise_of_org_in(tx, org_id).await?;
        if let Some(e) = ent {
            let key = format!("enterprise:{}", e.id);
            let flags = setting::<Flags>(tx, &key, kind::FLAGS)
                .await?
                .map(|(f, _)| f);
            if flags.as_ref().is_some_and(|f| f.ip) {
                let s = Self::ip_settings_in(tx, &key).await?;
                let ranges = Self::active_ranges(tx, &key, s.include_installed_app_ranges).await?;
                return Ok(Some((ranges, s.applies_to_installed_apps)));
            }
        }
        let key = format!("org:{org_id}");
        let flags = setting::<Flags>(tx, &key, kind::FLAGS)
            .await?
            .map(|(f, _)| f);
        if flags.is_some_and(|f| f.ip) {
            let s = Self::ip_settings_in(tx, &key).await?;
            let ranges = Self::active_ranges(tx, &key, s.include_installed_app_ranges).await?;
            return Ok(Some((ranges, s.applies_to_installed_apps)));
        }
        Ok(None)
    }

    // ---------------------------------------------------------------- domains

    fn valid_domain(d: &str) -> bool {
        d.len() <= 253
            && d.contains('.')
            && d.split('.').all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && !l.starts_with('-')
                    && !l.ends_with('-')
                    && l.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            })
    }

    /// The challenge record name for a domain of a scope.
    pub fn challenge_name(scope: &ResolvedScope, domain: &str) -> String {
        format!("_beehive-challenge-{}.{domain}", scope.name.to_lowercase())
    }

    /// Adds a domain and returns the DNS challenge to publish (or an approved-only domain).
    pub async fn add_domain(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        domain: &str,
        approved_only: bool,
    ) -> Result<VerifiedDomain> {
        let domain = domain.trim().trim_end_matches('.').to_lowercase();
        if !Self::valid_domain(&domain) {
            return Err(ForgeError::invalid(
                "invalid_domain",
                format!("`{domain}` is not a domain name"),
            ));
        }
        let key = scope.key();
        let token = self.forge.db.ids().random_hex(16)?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if !tx
            .scan(
                &DOMAINS,
                &Scan::scope(key.clone(), 1).key(Key::K1(domain.clone())),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!("domain {domain}")));
        }
        let now = tx.now_ms();
        let mut d = VerifiedDomain {
            id: tx.new_id()?,
            scope: Some(scope.to_proto()),
            domain: domain.clone(),
            created_at: Some(ts(now)),
            ..Default::default()
        };
        if approved_only {
            d.state = VerifiedDomainState::Approved as i32;
        } else {
            d.state = VerifiedDomainState::Pending as i32;
            d.dns_txt_record_name = Self::challenge_name(scope, &domain);
            d.dns_txt_record_value = format!("beehive-challenge={token}");
            d.challenge_expires_at = Some(ts(now + CHALLENGE_DAYS * DAY_MS));
        }
        tx.insert(&DOMAINS, &Row::new(key, d.id.clone()).k1(domain).msg(&d))
            .await?;
        self.record(&mut tx, scope, "enterprise.domain_add", &d.id, &(), &d)
            .await?;
        tx.commit().await?;
        Ok(d)
    }

    /// Looks up the challenge now and verifies the domain when it matches.
    pub async fn check_domain(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        domain_id: &str,
    ) -> Result<VerifiedDomain> {
        let key = scope.key();
        let d = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            get_or_404::<VerifiedDomain>(&mut tx, &DOMAINS, &key, domain_id, "domain")
                .await?
                .0
        };
        match VerifiedDomainState::try_from(d.state).unwrap_or(VerifiedDomainState::Unspecified) {
            VerifiedDomainState::Verified => return Ok(d),
            VerifiedDomainState::Approved => {
                return Err(ForgeError::invalid_state(
                    "approved domains are not verified",
                ));
            }
            _ => {}
        }
        let records = self.dns.txt(&d.dns_txt_record_name).await?;
        let matched = records
            .iter()
            .any(|r| common::ct_eq(r.trim().trim_matches('"'), &d.dns_txt_record_value));
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut d, row) =
            get_or_404::<VerifiedDomain>(&mut tx, &DOMAINS, &key, domain_id, "domain").await?;
        let before = d.clone();
        let now = tx.now_ms();
        d.last_checked_at = Some(ts(now));
        if matched {
            let others = tx
                .scan_msgs::<VerifiedDomain>(
                    &DOMAINS,
                    &Scan {
                        key: Some(Key::K1(d.domain.clone())),
                        limit: 100,
                        ..Scan::default()
                    },
                )
                .await?;
            if others
                .iter()
                .any(|(o, r)| r.scope != key && o.state == VerifiedDomainState::Verified as i32)
            {
                return Err(ForgeError::already_exists(format!(
                    "domain {} is verified by another account",
                    d.domain
                )));
            }
            d.state = VerifiedDomainState::Verified as i32;
            d.verified_at = Some(ts(now));
        } else if d
            .challenge_expires_at
            .as_ref()
            .is_some_and(|e| crate::ids::ts_ms(e) <= now)
        {
            d.state = VerifiedDomainState::Failed as i32;
        }
        tx.update(&DOMAINS, &row.msg(&d)).await?;
        if d.state != before.state {
            self.record(
                &mut tx,
                scope,
                "enterprise.domain_check",
                &d.id,
                &before,
                &d,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(d)
    }

    /// Lists domains.
    pub async fn domains(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<VerifiedDomain>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) =
            scan_page::<VerifiedDomain>(&mut tx, &DOMAINS, &scope.key(), None, after, limit)
                .await?;
        Ok((rows.into_iter().map(|(d, _)| d).collect(), next))
    }

    /// Deletes a domain.
    pub async fn delete_domain(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        domain_id: &str,
    ) -> Result<()> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (d, _) =
            get_or_404::<VerifiedDomain>(&mut tx, &DOMAINS, &key, domain_id, "domain").await?;
        tx.delete(&DOMAINS, &key, domain_id).await?;
        self.record(
            &mut tx,
            scope,
            "enterprise.domain_delete",
            domain_id,
            &d,
            &(),
        )
        .await?;
        tx.commit().await
    }

    /// Whether `domain` is verified for the org or its enterprise.
    pub async fn is_domain_verified(&self, cx: &Cx, org_id: &str, domain: &str) -> Result<bool> {
        let domain = domain.to_lowercase();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut keys = vec![format!("org:{org_id}")];
        if let Some(e) = Accounts::enterprise_of_org_in(&mut tx, org_id).await? {
            keys.push(format!("enterprise:{}", e.id));
        }
        for k in keys {
            let rows = tx
                .scan_msgs::<VerifiedDomain>(
                    &DOMAINS,
                    &Scan::scope(k, 1).key(Key::K1(domain.clone())),
                )
                .await?;
            if rows
                .iter()
                .any(|(d, _)| d.state == VerifiedDomainState::Verified as i32)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Domain-related settings of a scope.
    pub async fn domain_settings(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
    ) -> Result<VerifiedDomainSettings> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut s = setting::<VerifiedDomainSettings>(&mut tx, &scope.key(), kind::DOMAINS)
            .await?
            .map(|(s, _)| s)
            .unwrap_or_default();
        s.scope = Some(scope.to_proto());
        Ok(s)
    }

    /// Changes domain settings. Restricting notifications needs one verified or approved
    /// domain (`INVALID_STATE`).
    pub async fn update_domain_settings(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        restrict_notifications: Option<bool>,
    ) -> Result<VerifiedDomainSettings> {
        let key = scope.key();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let before = setting::<VerifiedDomainSettings>(&mut tx, &key, kind::DOMAINS)
            .await?
            .map(|(s, _)| s)
            .unwrap_or_default();
        let mut s = before.clone();
        if let Some(r) = restrict_notifications {
            if r {
                let ok = scan_all::<VerifiedDomain>(&mut tx, &DOMAINS, &key, None)
                    .await?
                    .iter()
                    .any(|(d, _)| {
                        d.state == VerifiedDomainState::Verified as i32
                            || d.state == VerifiedDomainState::Approved as i32
                    });
                if !ok {
                    return Err(ForgeError::invalid_state(
                        "restricting notifications needs a verified or approved domain",
                    ));
                }
            }
            s.restrict_notifications = r;
        }
        put_setting(&mut tx, &key, kind::DOMAINS, &s).await?;
        self.record(
            &mut tx,
            scope,
            "enterprise.domain_settings_update",
            &key,
            &before,
            &s,
        )
        .await?;
        tx.commit().await?;
        s.scope = Some(scope.to_proto());
        Ok(s)
    }

    // ---------------------------------------------------------------- two-factor

    async fn two_factor_in(tx: &mut Tx, key: &str) -> Result<EnterpriseTwoFactorRequirement> {
        Ok(
            setting::<EnterpriseTwoFactorRequirement>(tx, key, kind::TWO_FACTOR)
                .await?
                .map(|(s, _)| s)
                .unwrap_or_default(),
        )
    }

    /// The two-factor requirement of a scope with the current noncompliant count.
    pub async fn two_factor(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
    ) -> Result<EnterpriseTwoFactorRequirement> {
        let mut r = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::two_factor_in(&mut tx, &scope.key()).await?
        };
        r.scope = Some(scope.to_proto());
        r.noncompliant_count = if r.required {
            self.noncompliant(cx, scope, r.secure_methods_only)
                .await?
                .len() as u32
        } else {
            0
        };
        Ok(r)
    }

    /// Members and outside collaborators of the scope without (secure) two-factor.
    pub async fn noncompliant(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        secure_only: bool,
    ) -> Result<Vec<EnterpriseTwoFactorMember>> {
        let mut out = Vec::new();
        for org_id in &scope.org_ids {
            let org = self.forge.orgs.org_any(cx, org_id).await?;
            let mut after = String::new();
            loop {
                let (page, next) = self.forge.orgs.members(cx, org_id, &after, 500).await?;
                for m in page {
                    let Some(p) = m.principal else { continue };
                    if !self.compliant(&p.id, secure_only).await {
                        out.push(EnterpriseTwoFactorMember {
                            principal: Some(p),
                            org: org.name.clone(),
                            outside_collaborator: false,
                        });
                    }
                }
                if next.is_empty() {
                    break;
                }
                after = next;
            }
            for oc in self.forge.orgs.outside_collaborators(cx, org_id).await? {
                if let Some(p) = oc.principal
                    && !self.compliant(&p.id, secure_only).await
                {
                    out.push(EnterpriseTwoFactorMember {
                        principal: Some(p),
                        org: org.name.clone(),
                        outside_collaborator: true,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            let k = |m: &EnterpriseTwoFactorMember| {
                (
                    m.org.clone(),
                    m.principal
                        .as_ref()
                        .map(|p| p.id.clone())
                        .unwrap_or_default(),
                )
            };
            k(a).cmp(&k(b))
        });
        Ok(out)
    }

    async fn compliant(&self, principal_id: &str, secure_only: bool) -> bool {
        if secure_only {
            self.two_factor.has_secure_two_factor(principal_id).await
        } else {
            self.two_factor.has_two_factor(principal_id).await
        }
    }

    /// Requires (or stops requiring) two-factor. With `remove_noncompliant`, members and
    /// outside collaborators without it are removed now (the last owner of an org stays; it is
    /// blocked at admission instead); returns the removed principal ids.
    pub async fn update_two_factor(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        required: Option<bool>,
        secure_only: Option<bool>,
        remove_noncompliant: bool,
    ) -> Result<(EnterpriseTwoFactorRequirement, Vec<String>)> {
        let key = scope.key();
        let (before, mut r) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let b = Self::two_factor_in(&mut tx, &key).await?;
            (b.clone(), b)
        };
        if let Some(v) = required {
            if v && !r.required {
                r.required_since = Some(ts(self.forge.db.now_ms()));
            }
            r.required = v;
        }
        if let Some(v) = secure_only {
            r.secure_methods_only = v;
        }
        let mut removed = Vec::new();
        if r.required && remove_noncompliant {
            let mut org_ids_by_name = BTreeMap::new();
            for id in &scope.org_ids {
                org_ids_by_name.insert(self.forge.orgs.org_any(cx, id).await?.name, id.clone());
            }
            for m in self.noncompliant(cx, scope, r.secure_methods_only).await? {
                let Some(p) = m.principal else { continue };
                let org_id = org_ids_by_name
                    .get(&m.org)
                    .cloned()
                    .ok_or_else(|| ForgeError::internal("noncompliant member of an unknown org"))?;
                if m.outside_collaborator {
                    let repos = self
                        .forge
                        .orgs
                        .outside_collaborators(cx, &org_id)
                        .await?
                        .into_iter()
                        .find(|o| o.principal.as_ref().is_some_and(|x| x.id == p.id))
                        .map(|o| o.repo_ids)
                        .unwrap_or_default();
                    for repo in repos {
                        self.forge
                            .orgs
                            .remove_collaborator(cx, &repo, &p.id)
                            .await?;
                    }
                    removed.push(p.id);
                } else {
                    match self
                        .forge
                        .orgs
                        .set_member(cx, &org_id, p.clone(), bee_proto::v1::Role::Unspecified)
                        .await
                    {
                        Ok(_) => removed.push(p.id),
                        Err(e) if e.reason == bee_proto::v1::ErrorReason::InvalidState => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        put_setting(&mut tx, &key, kind::TWO_FACTOR, &r).await?;
        let (req, sec) = (r.required, r.secure_methods_only);
        update_flags(&mut tx, &key, |f| {
            f.two_factor = req;
            f.two_factor_secure_only = sec;
        })
        .await?;
        self.record(
            &mut tx,
            scope,
            "enterprise.two_factor_update",
            &key,
            &before,
            &r,
        )
        .await?;
        tx.commit().await?;
        let r = self.two_factor(cx, scope).await?;
        removed.sort();
        removed.dedup();
        Ok((r, removed))
    }

    // ---------------------------------------------------------------- admission

    /// Admits or refuses a request (see the module docs). Pass every org the request touches.
    pub async fn admit(&self, cx: &Cx, req: &AdmissionRequest) -> Result<()> {
        for id in &req.principals {
            if let Some(s) = self.accounts.principal_state(cx, id).await? {
                return Err(ForgeError::permission_denied(format!(
                    "{id} is {}: {}",
                    s.state, s.reason
                ))
                .with_subcode("principal_blocked"));
            }
        }
        let orgs: BTreeSet<&String> = req.org_ids.iter().collect();
        for org_id in orgs {
            self.admit_org(cx, req, org_id).await?;
        }
        Ok(())
    }

    /// The scopes (org, then its enterprise) of `org_id` that enforce SSO or two-factor, with
    /// their flags.
    async fn enforcing(&self, cx: &Cx, org_id: &str) -> Result<Vec<(String, Flags)>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut scopes = vec![format!("org:{org_id}")];
        if let Some(e) = Accounts::enterprise_of_org_in(&mut tx, org_id).await? {
            scopes.push(format!("enterprise:{}", e.id));
        }
        let mut out = Vec::new();
        for k in scopes {
            if let Some((f, _)) = setting::<Flags>(&mut tx, &k, kind::FLAGS).await?
                && (f.sso_enforced || f.two_factor)
            {
                out.push((k, f));
            }
        }
        Ok(out)
    }

    async fn admit_org(&self, cx: &Cx, req: &AdmissionRequest, org_id: &str) -> Result<()> {
        let rule = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            Self::ip_rule(&mut tx, org_id).await?
        };
        if let Some((ranges, apps)) = rule
            && (!req.app_token || apps)
        {
            let allowed = req
                .ip
                .parse::<IpAddr>()
                .is_ok_and(|ip| ranges.iter().any(|r| r.contains(ip)));
            if !allowed {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::IpNotAllowed,
                    format!("address {:?} is not on the allow list of this org", req.ip),
                ));
            }
        }
        let enforcing = self.enforcing(cx, org_id).await?;
        self.check_members(cx, &req.principals, org_id, &enforcing, true)
            .await
    }

    /// SSO sessions (and, with `two_factor`, 2FA) of the org members among `principals`.
    async fn check_members(
        &self,
        cx: &Cx,
        principals: &[String],
        org_id: &str,
        enforcing: &[(String, Flags)],
        two_factor: bool,
    ) -> Result<()> {
        if enforcing.is_empty() {
            return Ok(());
        }
        for id in principals {
            if self.forge.orgs.member(cx, org_id, id).await?.is_none() {
                continue;
            }
            for (key, f) in enforcing {
                if f.sso_enforced {
                    let mut tx = self.db.begin(SURFACE, cx).await?;
                    if !super::sso::session_live(&mut tx, key, id).await? {
                        return Err(ForgeError::new(
                            bee_proto::v1::ErrorReason::SsoRequired,
                            format!("{id} needs a live SAML SSO session to use {org_id}"),
                        ));
                    }
                }
                if two_factor && f.two_factor && !self.compliant(id, f.two_factor_secure_only).await
                {
                    return Err(ForgeError::new(
                        bee_proto::v1::ErrorReason::TwoFactorRequired,
                        format!("{id} must enable two-factor authentication to use {org_id}"),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl crate::enterprise::apps::SsoGate for Network {
    async fn check_sso(&self, cx: &Cx, principal_id: &str, org_ids: &[String]) -> Result<()> {
        for org_id in org_ids {
            let enforcing = self.enforcing(cx, org_id).await?;
            self.check_members(
                cx,
                std::slice::from_ref(&principal_id.to_string()),
                org_id,
                &enforcing,
                false,
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl crate::enterprise::apps::DomainCheck for Network {
    async fn is_verified(&self, cx: &Cx, org_id: &str, domain: &str) -> Result<bool> {
        self.is_domain_verified(cx, org_id, domain).await
    }

    async fn two_factor_required(&self, cx: &Cx, org_id: &str) -> Result<bool> {
        Ok(self
            .enforcing(cx, org_id)
            .await?
            .iter()
            .any(|(_, f)| f.two_factor))
    }
}

#[async_trait]
impl IpPolicy for Network {
    async fn check_ip(&self, cx: &Cx, org_id: &str, ip: &str, app_token: bool) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        if let Some((ranges, apps)) = Self::ip_rule(&mut tx, org_id).await?
            && (!app_token || apps)
        {
            let allowed = ip
                .parse::<IpAddr>()
                .is_ok_and(|a| ranges.iter().any(|r| r.contains(a)));
            if !allowed {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::IpNotAllowed,
                    format!("address {ip:?} is not on the allow list of this org"),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_matching() {
        let c = Cidr::parse("203.0.113.77/24").unwrap();
        assert_eq!(c.to_string(), "203.0.113.0/24");
        assert!(c.contains("203.0.113.5".parse().unwrap()));
        assert!(!c.contains("203.0.114.5".parse().unwrap()));
        assert!(!c.contains("2001:db8::1".parse().unwrap()));
        let v6 = Cidr::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:ffff::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));
        let host = Cidr::parse("198.51.100.9").unwrap();
        assert!(host.contains("198.51.100.9".parse().unwrap()));
        assert!(!host.contains("198.51.100.10".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("nope").is_err());
        assert!(
            Cidr::parse("::ffff:10.1.2.3/104")
                .unwrap()
                .contains("10.9.9.9".parse().unwrap())
        );
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
    }

    #[test]
    fn domains_validate() {
        assert!(Network::valid_domain("example.com"));
        assert!(Network::valid_domain("a-b.example.co.uk"));
        assert!(!Network::valid_domain("localhost"));
        assert!(!Network::valid_domain("-bad.example.com"));
        assert!(!Network::valid_domain("exa mple.com"));
    }
}
