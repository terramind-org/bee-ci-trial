//! Sponsors (PRODUCT §4.16, F416 sponsors): maintainee profiles, tiers, sponsorship records,
//! sponsor-only repo access, payout bookkeeping and `FUNDING.yml`.
//!
//! The forge records who sponsors whom and keeps the state machine; money moves through the
//! [`SponsorPayments`](super::ports::SponsorPayments) port.
//!
//! # Sponsorship states
//!
//! ```text
//! create ─┬─ checkout URL ─▶ PENDING ─ confirm_payment(ok) ─▶ ACTIVE ─ cancel ─▶ CANCELLING ─ period end ─▶ ENDED
//!         │                          └ confirm_payment(fail) ─▶ FAILED          (monthly)
//!         └─ no checkout ──────────────────────────────────▶ ACTIVE ─ cancel (one-time) ─▶ ENDED
//! ```
//!
//! A pending sponsorship that is cancelled ends at once. Active one-time sponsorships and
//! cancelling ones end at their period end (`maintain`). Tiers go `DRAFT` → `PUBLISHED` →
//! `RETIRED`; only drafts change amount; a tier with live sponsorships is retired, not
//! deleted.
//!
//! # Sponsor-only repos
//!
//! A tier may name repos; a principal with an `ACTIVE` or `CANCELLING` sponsorship (before its
//! period end) of that tier holds `read` on them through [`SponsorAccess`], an
//! [`AccessSource`](crate::orgs::AccessSource) the forge registers with the orgs module.
//!
//! # Privacy
//!
//! Private sponsorships are visible to their two parties only and are left out of public
//! counts and totals.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    ObjectId, Principal, Repo, RepoRef, RepoRole, SponsorContentAccess, SponsorFiscalHost,
    SponsorFrequency, SponsorFundingFile, SponsorPayout, SponsorPayoutSummary, SponsorProfile,
    SponsorTier, SponsorTierState, Sponsorship, SponsorshipRole, SponsorshipState, SubjectKind,
};
use prost::Message;

use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::module::EventKind;
use crate::orgs::AccessSource;
use crate::store::{Db, Index, Key, Row, Scan, TableDef, Tx};

use super::SURFACE;
use super::common::{draft, emit, max_len, page_vec, ref_repo_id, repo_ref_id, required, subject};
use super::ports::{ContentVcs, SponsorPayments, VcsRepo, read_file};
use super::repofiles::parse_funding_with_base;

/// Sponsor profiles, keyed by owner key.
pub static PROFILES: TableDef = TableDef {
    name: "forge_sponsor_profiles",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[],
};
/// Tiers, scoped by owner key.
pub static TIERS: TableDef = TableDef {
    name: "forge_sponsor_tiers",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};
/// Sponsorships, scoped by maintainee key; `k1` sponsor key, `k2` tier id.
pub static SPONSORSHIPS: TableDef = TableDef {
    name: "forge_sponsorships",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeK1, Index::K1, Index::ScopeK2, Index::ScopeN1],
};
/// Payout records, scoped by owner key; `n1` payout or scheduled time.
pub static PAYOUTS: TableDef = TableDef {
    name: "forge_sponsor_payouts",
    surface: SURFACE,
    unique_k1: false,
    unique_k2: false,
    indexes: &[Index::ScopeN1],
};

/// Sponsor event kinds.
pub const EVENTS: &[EventKind] = &[
    EventKind::new("sponsor.profile_updated", "A sponsor profile changed"),
    EventKind::new(
        "sponsor.tier_changed",
        "A tier was created, changed or deleted",
    ),
    EventKind::new("sponsor.created", "A sponsorship started"),
    EventKind::new("sponsor.state_changed", "A sponsorship changed state"),
    EventKind::new("sponsor.payout_recorded", "A payout was recorded"),
];

/// Most live (non-retired) tiers per owner.
pub const MAX_TIERS: usize = 20;
/// Smallest sponsorship amount (cents).
pub const MIN_CENTS: u64 = 100;

const DAY_MS: u64 = 86_400_000;
const PERIOD_MS: u64 = 30 * DAY_MS;

/// Someone who can be sponsored: a user or an org.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Owner {
    /// `user:<id>` or `org:<id>`; partition key of their sponsor data.
    pub key: String,
    /// The principal shown (an org is shown with its id and name).
    pub principal: Principal,
    /// Login or org name.
    pub login: String,
}

impl Owner {
    /// A user owner.
    pub fn user(p: &Principal, login: &str) -> Owner {
        Owner {
            key: format!("user:{}", p.id),
            principal: p.clone(),
            login: login.to_string(),
        }
    }

    /// An org owner.
    pub fn org(org_id: &str, name: &str) -> Owner {
        Owner {
            key: format!("org:{org_id}"),
            principal: Principal {
                id: org_id.to_string(),
                display: name.to_string(),
                ..Default::default()
            },
            login: name.to_string(),
        }
    }
}

/// A sponsor profile's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct ProfileRec {
    /// The profile (computed fields are filled on read).
    #[prost(message, optional, tag = "1")]
    pub profile: Option<SponsorProfile>,
    /// The fiscal host, if any.
    #[prost(message, optional, tag = "2")]
    pub fiscal_host: Option<SponsorFiscalHost>,
}

/// A sponsorship's stored record.
#[derive(Clone, PartialEq, Message)]
pub struct SponsorshipRec {
    /// The sponsorship.
    #[prost(message, optional, tag = "1")]
    pub sponsorship: Option<Sponsorship>,
}

fn state_of(s: &Sponsorship) -> SponsorshipState {
    SponsorshipState::try_from(s.state).unwrap_or(SponsorshipState::Unspecified)
}

/// Whether a sponsorship currently grants its tier's benefits.
pub fn grants_access(s: &Sponsorship, now_ms: u64) -> bool {
    matches!(
        state_of(s),
        SponsorshipState::Active | SponsorshipState::Cancelling
    ) && s.current_period_end_at.as_ref().map(ts_ms).unwrap_or(0) > now_ms
}

/// What a new sponsorship asks for.
#[derive(Clone, Debug, Default)]
pub struct NewSponsorship {
    /// Tier id (empty with an amount for a custom sponsorship).
    pub tier_id: String,
    /// Amount in cents (custom amounts, or above a tier that allows it).
    pub amount_cents: u64,
    /// Frequency for custom sponsorships.
    pub frequency: i32,
    /// Hide from the public list.
    pub private: bool,
    /// Message to the maintainee.
    pub note: String,
}

/// Tier fields for creation.
#[derive(Clone, Debug, Default)]
pub struct NewTier {
    /// Name.
    pub name: String,
    /// Markdown description.
    pub description: String,
    /// Amount in cents.
    pub amount_cents: u64,
    /// Frequency.
    pub frequency: i32,
    /// Sponsors may pay more.
    pub custom_amount: bool,
    /// Repo ids sponsors can read.
    pub repo_ids: Vec<String>,
    /// Welcome message.
    pub welcome: String,
    /// Publish at once.
    pub publish: bool,
}

/// A partial tier update.
#[derive(Clone, Debug, Default)]
pub struct TierPatch {
    /// Name.
    pub name: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Amount (drafts only).
    pub amount_cents: Option<u64>,
    /// Custom amounts allowed.
    pub custom_amount: Option<bool>,
    /// Replace the sponsor-only repos with these ids.
    pub repo_ids: Option<Vec<String>>,
    /// Welcome message.
    pub welcome: Option<String>,
    /// State transition.
    pub state: Option<i32>,
}

/// Sponsors over the store and ports.
pub struct Sponsors {
    db: Db,
    payments: Arc<dyn SponsorPayments>,
    vcs: Arc<dyn ContentVcs>,
    base_url: String,
}

impl Sponsors {
    /// Sponsors over `db`, the payment port and the content VCS (for `FUNDING.yml`).
    pub fn new(
        db: Db,
        payments: Arc<dyn SponsorPayments>,
        vcs: Arc<dyn ContentVcs>,
        base_url: &str,
    ) -> Sponsors {
        Sponsors {
            db,
            payments,
            vcs,
            base_url: base_url.to_string(),
        }
    }

    async fn announce(
        tx: &mut Tx,
        kind: &str,
        audit: &str,
        id: &str,
        payload: serde_json::Value,
    ) -> Result<()> {
        emit(
            tx,
            kind,
            "",
            "",
            subject(SubjectKind::User, "", id),
            payload.clone(),
        )
        .await?;
        tx.audit(draft(audit, "", "", SubjectKind::User, id, &payload))
            .await
    }

    fn valid_currency(c: &str) -> bool {
        c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase())
    }

    // ---- profile ----------------------------------------------------------------------

    async fn rec(tx: &mut Tx, owner: &Owner) -> Result<(ProfileRec, Option<Row>)> {
        Ok(
            match tx.get_msg::<ProfileRec>(&PROFILES, "", &owner.key).await? {
                Some((r, row)) => (r, Some(row)),
                None => (
                    ProfileRec {
                        profile: Some(SponsorProfile {
                            maintainee: Some(owner.principal.clone()),
                            login: owner.login.clone(),
                            currency: "USD".into(),
                            ..Default::default()
                        }),
                        fiscal_host: None,
                    },
                    None,
                ),
            },
        )
    }

    async fn sponsorships_of(tx: &mut Tx, owner_key: &str) -> Result<Vec<Sponsorship>> {
        Ok(tx
            .scan_msgs::<SponsorshipRec>(
                &SPONSORSHIPS,
                &Scan::scope(owner_key, crate::store::MAX_SCAN),
            )
            .await?
            .into_iter()
            .filter_map(|(r, _)| r.sponsorship)
            .collect())
    }

    async fn tiers_of(tx: &mut Tx, owner_key: &str) -> Result<Vec<SponsorTier>> {
        Ok(tx
            .scan_msgs::<SponsorTier>(&TIERS, &Scan::scope(owner_key, 1000).by_n1())
            .await?
            .into_iter()
            .map(|(t, _)| t)
            .collect())
    }

    fn tier_view(mut t: SponsorTier, sps: &[Sponsorship], now: u64) -> SponsorTier {
        t.active_sponsor_count = sps
            .iter()
            .filter(|s| s.tier_id == t.id && grants_access(s, now))
            .count() as u32;
        t
    }

    /// A sponsor profile; `as_owner` shows private sponsorships in the totals.
    pub async fn profile(&self, cx: &Cx, owner: &Owner, as_owner: bool) -> Result<SponsorProfile> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::rec(&mut tx, owner).await?;
        let mut p = rec.profile.unwrap_or_default();
        let now = tx.now_ms();
        let sps = Self::sponsorships_of(&mut tx, &owner.key).await?;
        let live: Vec<&Sponsorship> = sps
            .iter()
            .filter(|s| grants_access(s, now) && (as_owner || !s.private))
            .collect();
        p.monthly_total_cents = live
            .iter()
            .filter(|s| s.frequency == SponsorFrequency::Monthly as i32)
            .map(|s| s.amount_cents)
            .sum();
        let mut sponsors: Vec<&str> = live
            .iter()
            .filter_map(|s| s.sponsor.as_ref().map(|p| p.id.as_str()))
            .collect();
        sponsors.sort_unstable();
        sponsors.dedup();
        p.sponsor_count = sponsors.len() as u32;
        p.tiers = Self::tiers_of(&mut tx, &owner.key)
            .await?
            .into_iter()
            .filter(|t| t.state == SponsorTierState::Published as i32)
            .map(|t| Self::tier_view(t, &sps, now))
            .collect();
        p.has_fiscal_host = rec.fiscal_host.is_some();
        p.maintainee = Some(owner.principal.clone());
        p.login = owner.login.clone();
        Ok(p)
    }

    /// Updates a sponsor profile; enabling needs at least one published tier.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_profile(
        &self,
        cx: &Cx,
        owner: &Owner,
        enabled: Option<bool>,
        introduction: Option<&str>,
        goal_cents: Option<u64>,
        goal_description: Option<&str>,
        currency: Option<&str>,
    ) -> Result<SponsorProfile> {
        if let Some(i) = introduction {
            max_len("introduction", i, 5000)?;
        }
        if let Some(g) = goal_description {
            max_len("goal_description", g, 500)?;
        }
        if let Some(c) = currency
            && !Self::valid_currency(c)
        {
            return Err(ForgeError::invalid(
                "invalid_currency",
                "currency is a three-letter ISO 4217 code in capitals",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::rec(&mut tx, owner).await?;
        let mut p = rec.profile.clone().unwrap_or_default();
        let now = tx.now_ms();
        if let Some(e) = enabled {
            if e && !p.enabled {
                let tiers = Self::tiers_of(&mut tx, &owner.key).await?;
                if !tiers
                    .iter()
                    .any(|t| t.state == SponsorTierState::Published as i32)
                {
                    return Err(ForgeError::invalid_state(
                        "publish a tier before enabling sponsorships",
                    )
                    .with_subcode("no_published_tier"));
                }
            }
            if p.enabled != e && p.created_at.is_none() {
                p.created_at = Some(ts(now));
            }
            p.enabled = e;
        }
        if let Some(i) = introduction {
            p.introduction = i.to_string();
        }
        if let Some(g) = goal_cents {
            p.goal_cents = g;
        }
        if let Some(g) = goal_description {
            p.goal_description = g.to_string();
        }
        if let Some(c) = currency {
            p.currency = c.to_string();
        }
        rec.profile = Some(p.clone());
        let new_row = match row {
            Some(r) => r.msg(&rec),
            None => Row::new("", owner.key.clone()).msg(&rec),
        };
        tx.put(&PROFILES, &new_row).await?;
        Self::announce(
            &mut tx,
            "sponsor.profile_updated",
            "sponsor.profile_update",
            &owner.principal.id,
            serde_json::json!({"owner": owner.key, "enabled": p.enabled}),
        )
        .await?;
        tx.commit().await?;
        self.profile(cx, owner, true).await
    }

    /// Sets the fiscal host of an owner (admin tooling; there is no RPC for it).
    pub async fn set_fiscal_host(
        &self,
        cx: &Cx,
        owner: &Owner,
        host: Option<SponsorFiscalHost>,
    ) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut rec, row) = Self::rec(&mut tx, owner).await?;
        rec.fiscal_host = host;
        let new_row = match row {
            Some(r) => r.msg(&rec),
            None => Row::new("", owner.key.clone()).msg(&rec),
        };
        tx.put(&PROFILES, &new_row).await?;
        Self::announce(
            &mut tx,
            "sponsor.profile_updated",
            "sponsor.fiscal_host_set",
            &owner.principal.id,
            serde_json::json!({"owner": owner.key, "fiscal_host": rec.fiscal_host.is_some()}),
        )
        .await?;
        tx.commit().await
    }

    /// The fiscal host of an owner.
    pub async fn fiscal_host(&self, cx: &Cx, owner: &Owner) -> Result<SponsorFiscalHost> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Self::rec(&mut tx, owner)
            .await?
            .0
            .fiscal_host
            .ok_or_else(|| ForgeError::not_found(format!("fiscal host of {}", owner.login)))
    }

    // ---- tiers ------------------------------------------------------------------------

    fn check_tier_fields(name: &str, description: &str, welcome: &str) -> Result<()> {
        required("name", name)?;
        max_len("name", name, 100)?;
        max_len("description", description, 2000)?;
        max_len("welcome_message", welcome, 2000)
    }

    /// Creates a tier (draft, or published with `publish`).
    pub async fn create_tier(&self, cx: &Cx, owner: &Owner, n: NewTier) -> Result<SponsorTier> {
        Self::check_tier_fields(&n.name, &n.description, &n.welcome)?;
        if n.amount_cents < MIN_CENTS {
            return Err(ForgeError::invalid(
                "amount_too_small",
                format!("amount_cents is at least {MIN_CENTS}"),
            ));
        }
        let frequency = if n.frequency == 0 {
            SponsorFrequency::Monthly as i32
        } else {
            n.frequency
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let tiers = Self::tiers_of(&mut tx, &owner.key).await?;
        let live = tiers
            .iter()
            .filter(|t| t.state != SponsorTierState::Retired as i32)
            .count();
        if live >= MAX_TIERS {
            return Err(ForgeError::too_large(
                "sponsor_tiers",
                MAX_TIERS as u64,
                live as u64 + 1,
            ));
        }
        if tiers.iter().any(|t| {
            t.state != SponsorTierState::Retired as i32
                && t.name.eq_ignore_ascii_case(n.name.trim())
        }) {
            return Err(ForgeError::already_exists(format!("tier {}", n.name)));
        }
        let now = tx.now_ms();
        let t = SponsorTier {
            id: tx.new_id()?,
            name: n.name.trim().to_string(),
            description: n.description,
            amount_cents: n.amount_cents,
            frequency,
            state: if n.publish {
                SponsorTierState::Published
            } else {
                SponsorTierState::Draft
            } as i32,
            custom_amount: n.custom_amount,
            sponsor_only_repos: n.repo_ids.iter().filter_map(|r| repo_ref_id(r)).collect(),
            welcome_message: n.welcome,
            created_at: Some(ts(now)),
            updated_at: Some(ts(now)),
            ..Default::default()
        };
        tx.insert(
            &TIERS,
            &Row::new(owner.key.clone(), t.id.clone())
                .n1(n.amount_cents as i64)
                .msg(&t),
        )
        .await?;
        Self::announce(
            &mut tx,
            "sponsor.tier_changed",
            "sponsor.tier_create",
            &owner.principal.id,
            serde_json::json!({"owner": owner.key, "tier": &t, "action": "created"}),
        )
        .await?;
        tx.commit().await?;
        Ok(t)
    }

    /// Tiers of an owner: published ones, plus drafts (`drafts`, owner only) and retired ones.
    pub async fn list_tiers(
        &self,
        cx: &Cx,
        owner: &Owner,
        drafts: bool,
        retired: bool,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<SponsorTier>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let sps = Self::sponsorships_of(&mut tx, &owner.key).await?;
        let tiers: Vec<SponsorTier> = Self::tiers_of(&mut tx, &owner.key)
            .await?
            .into_iter()
            .filter(|t| {
                match SponsorTierState::try_from(t.state).unwrap_or(SponsorTierState::Unspecified) {
                    SponsorTierState::Published => true,
                    SponsorTierState::Draft => drafts,
                    SponsorTierState::Retired => retired,
                    SponsorTierState::Unspecified => false,
                }
            })
            .map(|t| Self::tier_view(t, &sps, now))
            .collect();
        page_vec(tiers, token, limit)
    }

    async fn find_tier(tx: &mut Tx, owner_key: &str, id: &str) -> Result<(SponsorTier, Row)> {
        tx.get_msg::<SponsorTier>(&TIERS, owner_key, id)
            .await?
            .ok_or_else(|| ForgeError::not_found(format!("tier {id}")))
    }

    /// Applies a tier patch.
    pub async fn update_tier(
        &self,
        cx: &Cx,
        owner: &Owner,
        id: &str,
        p: TierPatch,
    ) -> Result<SponsorTier> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut t, mut row) = Self::find_tier(&mut tx, &owner.key, id).await?;
        let state = SponsorTierState::try_from(t.state).unwrap_or(SponsorTierState::Unspecified);
        if let Some(n) = &p.name {
            Self::check_tier_fields(n, "", "")?;
            t.name = n.trim().to_string();
        }
        if let Some(d) = &p.description {
            max_len("description", d, 2000)?;
            t.description = d.clone();
        }
        if let Some(a) = p.amount_cents {
            if state != SponsorTierState::Draft {
                return Err(ForgeError::invalid_state(
                    "only a draft tier changes amount; create a new tier instead",
                )
                .with_subcode("amount_locked"));
            }
            if a < MIN_CENTS {
                return Err(ForgeError::invalid(
                    "amount_too_small",
                    format!("amount_cents is at least {MIN_CENTS}"),
                ));
            }
            t.amount_cents = a;
            row.n1 = Some(a as i64);
        }
        if let Some(c) = p.custom_amount {
            t.custom_amount = c;
        }
        if let Some(r) = &p.repo_ids {
            t.sponsor_only_repos = r.iter().filter_map(|x| repo_ref_id(x)).collect();
        }
        if let Some(w) = &p.welcome {
            max_len("welcome_message", w, 2000)?;
            t.welcome_message = w.clone();
        }
        if let Some(s) = p.state {
            let to = SponsorTierState::try_from(s).unwrap_or(SponsorTierState::Unspecified);
            match (state, to) {
                (a, b) if a == b => {}
                (SponsorTierState::Draft, SponsorTierState::Published)
                | (SponsorTierState::Published, SponsorTierState::Retired)
                | (SponsorTierState::Draft, SponsorTierState::Retired) => {
                    t.state = to as i32;
                }
                _ => {
                    return Err(
                        ForgeError::invalid_state("tiers go draft, published, retired")
                            .with_subcode("bad_tier_transition"),
                    );
                }
            }
        }
        t.updated_at = Some(ts(tx.now_ms()));
        row.body = t.encode_to_vec();
        tx.update(&TIERS, &row).await?;
        Self::announce(
            &mut tx,
            "sponsor.tier_changed",
            "sponsor.tier_update",
            &owner.principal.id,
            serde_json::json!({"owner": owner.key, "tier": &t, "action": "updated"}),
        )
        .await?;
        let now = tx.now_ms();
        let sps = Self::sponsorships_of(&mut tx, &owner.key).await?;
        tx.commit().await?;
        Ok(Self::tier_view(t, &sps, now))
    }

    /// The owner key (`user:<id>` or `org:<id>`) a tier belongs to.
    pub async fn tier_owner_key(&self, cx: &Cx, tier_id: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx
            .scan(
                &TIERS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        rows.into_iter()
            .find(|r| r.id == tier_id)
            .map(|r| r.scope)
            .ok_or_else(|| ForgeError::not_found(format!("tier {tier_id}")))
    }

    /// Deletes a tier nobody sponsors through; otherwise it must be retired.
    pub async fn delete_tier(&self, cx: &Cx, owner: &Owner, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (t, _) = Self::find_tier(&mut tx, &owner.key, id).await?;
        let now = tx.now_ms();
        let sps = Self::sponsorships_of(&mut tx, &owner.key).await?;
        if sps.iter().any(|s| {
            s.tier_id == id
                && (grants_access(s, now) || matches!(state_of(s), SponsorshipState::Pending))
        }) {
            return Err(ForgeError::invalid_state(
                "the tier has active sponsorships; retire it instead",
            )
            .with_subcode("tier_in_use"));
        }
        tx.delete(&TIERS, &owner.key, id).await?;
        Self::announce(
            &mut tx,
            "sponsor.tier_changed",
            "sponsor.tier_delete",
            &owner.principal.id,
            serde_json::json!({"owner": owner.key, "tier": &t, "action": "deleted"}),
        )
        .await?;
        tx.commit().await
    }

    // ---- sponsorships -----------------------------------------------------------------

    /// Starts a sponsorship of `maintainee` by `sponsor`; returns it and the checkout URL
    /// when payment is pending.
    pub async fn create_sponsorship(
        &self,
        cx: &Cx,
        maintainee: &Owner,
        sponsor: &Owner,
        n: NewSponsorship,
    ) -> Result<(Sponsorship, String)> {
        max_len("note", &n.note, 500)?;
        if sponsor.principal.id == maintainee.principal.id {
            return Err(ForgeError::invalid_state("you cannot sponsor yourself")
                .with_subcode("self_sponsor"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = Self::rec(&mut tx, maintainee).await?;
        if !rec.profile.as_ref().is_some_and(|p| p.enabled) {
            return Err(ForgeError::invalid_state(format!(
                "{} is not accepting sponsorships",
                maintainee.login
            ))
            .with_subcode("sponsors_disabled"));
        }
        let (amount, frequency, tier_id) = if n.tier_id.is_empty() {
            if n.amount_cents < MIN_CENTS {
                return Err(ForgeError::invalid(
                    "amount_too_small",
                    format!("amount_cents is at least {MIN_CENTS}"),
                ));
            }
            let f = if n.frequency == 0 {
                SponsorFrequency::OneTime as i32
            } else {
                n.frequency
            };
            (n.amount_cents, f, String::new())
        } else {
            let (t, _) = Self::find_tier(&mut tx, &maintainee.key, &n.tier_id).await?;
            if t.state != SponsorTierState::Published as i32 {
                return Err(ForgeError::invalid_state("the tier is not published")
                    .with_subcode("tier_unavailable"));
            }
            let amount = if n.amount_cents == 0 {
                t.amount_cents
            } else if n.amount_cents == t.amount_cents
                || (n.amount_cents > t.amount_cents && t.custom_amount)
            {
                n.amount_cents
            } else {
                return Err(ForgeError::invalid(
                    "amount_mismatch",
                    "the amount must match the tier (or exceed it when the tier allows custom amounts)",
                ));
            };
            (amount, t.frequency, t.id)
        };
        let sps = Self::sponsorships_of(&mut tx, &maintainee.key).await?;
        if frequency == SponsorFrequency::Monthly as i32
            && sps.iter().any(|s| {
                s.sponsor.as_ref().map(|p| &p.id) == Some(&sponsor.principal.id)
                    && s.frequency == SponsorFrequency::Monthly as i32
                    && matches!(
                        state_of(s),
                        SponsorshipState::Pending
                            | SponsorshipState::Active
                            | SponsorshipState::Cancelling
                    )
            })
        {
            return Err(
                ForgeError::already_exists("a monthly sponsorship of this maintainer")
                    .with_subcode("already_sponsoring"),
            );
        }
        let now = tx.now_ms();
        let currency = rec
            .profile
            .as_ref()
            .map(|p| p.currency.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "USD".into());
        let mut s = Sponsorship {
            id: tx.new_id()?,
            sponsor: Some(sponsor.principal.clone()),
            maintainee: Some(maintainee.principal.clone()),
            tier_id,
            amount_cents: amount,
            currency,
            frequency,
            state: SponsorshipState::Pending as i32,
            private: n.private,
            note: n.note,
            created_at: Some(ts(now)),
            ..Default::default()
        };
        let checkout = self.payments.checkout(&s).await?;
        let url = match checkout {
            Some(u) => u,
            None => {
                s.state = SponsorshipState::Active as i32;
                s.current_period_end_at = Some(ts(now + PERIOD_MS));
                String::new()
            }
        };
        tx.insert(
            &SPONSORSHIPS,
            &Row::new(maintainee.key.clone(), s.id.clone())
                .k1(sponsor.key.clone())
                .k2(s.tier_id.clone())
                .n1(now as i64)
                .msg(&SponsorshipRec {
                    sponsorship: Some(s.clone()),
                }),
        )
        .await?;
        let payload = serde_json::json!({"sponsorship": {"id": s.id, "tier_id": s.tier_id, "amount_cents": s.amount_cents, "state": state_of(&s).as_str_name(), "private": s.private}, "maintainee": maintainee.key, "sponsor": sponsor.key});
        Self::announce(
            &mut tx,
            "sponsor.created",
            "sponsor.create",
            &maintainee.principal.id,
            payload,
        )
        .await?;
        tx.commit().await?;
        Ok((s, url))
    }

    async fn find(tx: &mut Tx, id: &str) -> Result<(Sponsorship, Row)> {
        let rows = tx
            .scan(
                &SPONSORSHIPS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        match rows.into_iter().find(|r| r.id == id) {
            Some(row) => Ok((
                row.decode::<SponsorshipRec>()?
                    .sponsorship
                    .unwrap_or_default(),
                row,
            )),
            None => Err(ForgeError::not_found(format!("sponsorship {id}"))),
        }
    }

    fn is_party(s: &Sponsorship, p: &str) -> bool {
        s.sponsor.as_ref().is_some_and(|x| x.id == p)
            || s.maintainee.as_ref().is_some_and(|x| x.id == p)
    }

    /// A sponsorship; private ones are visible to their parties (the caller, or an org they
    /// administer, passed in `parties`) only.
    pub async fn get(&self, cx: &Cx, id: &str, parties: &[String]) -> Result<Sponsorship> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (s, _) = Self::find(&mut tx, id).await?;
        if s.private && !parties.iter().any(|p| Self::is_party(&s, p)) {
            return Err(ForgeError::not_found(format!("sponsorship {id}")));
        }
        Ok(s)
    }

    /// Cancels a sponsorship (either party). Pending and one-time sponsorships end at once;
    /// active monthly ones end at their period end.
    pub async fn cancel(&self, cx: &Cx, id: &str, parties: &[String]) -> Result<Sponsorship> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut s, mut row) = {
            let (s, row) = Self::find(&mut tx, id).await?;
            let row = tx
                .get_for_update(&SPONSORSHIPS, &row.scope, &row.id)
                .await?
                .ok_or_else(|| ForgeError::not_found(format!("sponsorship {id}")))?;
            (s, row)
        };
        if !parties.iter().any(|p| Self::is_party(&s, p)) {
            return Err(ForgeError::not_found(format!("sponsorship {id}")));
        }
        let now = tx.now_ms();
        match state_of(&s) {
            SponsorshipState::Pending => {
                s.state = SponsorshipState::Ended as i32;
                s.ended_at = Some(ts(now));
            }
            SponsorshipState::Active if s.frequency == SponsorFrequency::Monthly as i32 => {
                s.state = SponsorshipState::Cancelling as i32;
            }
            SponsorshipState::Active => {
                s.state = SponsorshipState::Ended as i32;
                s.ended_at = Some(ts(now));
            }
            _ => {
                return Err(ForgeError::invalid_state("the sponsorship is not active")
                    .with_subcode("not_active"));
            }
        }
        self.payments.cancel(&s).await?;
        row.body = SponsorshipRec {
            sponsorship: Some(s.clone()),
        }
        .encode_to_vec();
        tx.update(&SPONSORSHIPS, &row).await?;
        let maintainee = s
            .maintainee
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        Self::announce(
            &mut tx,
            "sponsor.state_changed",
            "sponsor.cancel",
            &maintainee,
            serde_json::json!({"sponsorship_id": s.id, "state": state_of(&s).as_str_name()}),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// Records the payment provider's answer for a pending sponsorship.
    pub async fn confirm_payment(&self, cx: &Cx, id: &str, paid: bool) -> Result<Sponsorship> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut s, row) = Self::find(&mut tx, id).await?;
        let mut row = tx
            .get_for_update(&SPONSORSHIPS, &row.scope, &row.id)
            .await?
            .unwrap_or(row);
        if state_of(&s) != SponsorshipState::Pending {
            return Err(ForgeError::invalid_state(
                "the sponsorship is not awaiting payment",
            ));
        }
        let now = tx.now_ms();
        if paid {
            s.state = SponsorshipState::Active as i32;
            s.current_period_end_at = Some(ts(now + PERIOD_MS));
        } else {
            s.state = SponsorshipState::Failed as i32;
            s.ended_at = Some(ts(now));
        }
        row.body = SponsorshipRec {
            sponsorship: Some(s.clone()),
        }
        .encode_to_vec();
        tx.update(&SPONSORSHIPS, &row).await?;
        let maintainee = s
            .maintainee
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        Self::announce(&mut tx, "sponsor.state_changed", "sponsor.payment", &maintainee, serde_json::json!({"sponsorship_id": s.id, "paid": paid, "state": state_of(&s).as_str_name()})).await?;
        tx.commit().await?;
        Ok(s)
    }

    /// Extends an active monthly sponsorship by one period (the billing worker calls this on
    /// each successful charge).
    pub async fn renew(&self, cx: &Cx, id: &str) -> Result<Sponsorship> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut s, row) = Self::find(&mut tx, id).await?;
        let mut row = tx
            .get_for_update(&SPONSORSHIPS, &row.scope, &row.id)
            .await?
            .unwrap_or(row);
        if state_of(&s) != SponsorshipState::Active
            || s.frequency != SponsorFrequency::Monthly as i32
        {
            return Err(ForgeError::invalid_state(
                "only an active monthly sponsorship renews",
            ));
        }
        let end = s
            .current_period_end_at
            .as_ref()
            .map(ts_ms)
            .unwrap_or(0)
            .max(tx.now_ms());
        s.current_period_end_at = Some(ts(end + PERIOD_MS));
        row.body = SponsorshipRec {
            sponsorship: Some(s.clone()),
        }
        .encode_to_vec();
        tx.update(&SPONSORSHIPS, &row).await?;
        let maintainee = s
            .maintainee
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_default();
        Self::announce(
            &mut tx,
            "sponsor.state_changed",
            "sponsor.renew",
            &maintainee,
            serde_json::json!({"sponsorship_id": s.id, "period_end_ms": end + PERIOD_MS}),
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }

    /// Ends one-time and cancelling sponsorships past their period; returns how many.
    pub async fn maintain(&self, cx: &Cx) -> Result<usize> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let rows = tx
            .scan(
                &SPONSORSHIPS,
                &Scan {
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?;
        let mut n = 0;
        for mut row in rows {
            let mut s = row
                .decode::<SponsorshipRec>()?
                .sponsorship
                .unwrap_or_default();
            let over = s.current_period_end_at.as_ref().map(ts_ms).unwrap_or(0) <= now;
            let ends = over
                && (state_of(&s) == SponsorshipState::Cancelling
                    || (state_of(&s) == SponsorshipState::Active
                        && s.frequency == SponsorFrequency::OneTime as i32));
            if !ends {
                continue;
            }
            s.state = SponsorshipState::Ended as i32;
            s.ended_at = Some(ts(now));
            row.body = SponsorshipRec {
                sponsorship: Some(s.clone()),
            }
            .encode_to_vec();
            tx.update(&SPONSORSHIPS, &row).await?;
            let maintainee = s
                .maintainee
                .as_ref()
                .map(|p| p.id.clone())
                .unwrap_or_default();
            Self::announce(
                &mut tx,
                "sponsor.state_changed",
                "sponsor.end",
                &maintainee,
                serde_json::json!({"sponsorship_id": s.id, "state": "ENDED"}),
            )
            .await?;
            n += 1;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// Sponsorships by role: those `owner` receives (`MAINTAINEE`) or gives (`SPONSOR`),
    /// newest first. Private ones only for parties (`parties`).
    #[allow(clippy::too_many_arguments)]
    pub async fn list(
        &self,
        cx: &Cx,
        role: SponsorshipRole,
        owner: &Owner,
        include_ended: bool,
        include_private: bool,
        parties: &[String],
        token: &str,
        limit: usize,
    ) -> Result<(Vec<Sponsorship>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = if role == SponsorshipRole::Sponsor {
            tx.scan_msgs::<SponsorshipRec>(
                &SPONSORSHIPS,
                &Scan {
                    scope: None,
                    key: Some(Key::K1(owner.key.clone())),
                    limit: crate::store::MAX_SCAN,
                    ..Scan::default()
                },
            )
            .await?
        } else {
            tx.scan_msgs::<SponsorshipRec>(
                &SPONSORSHIPS,
                &Scan::scope(owner.key.clone(), crate::store::MAX_SCAN),
            )
            .await?
        };
        let mut v: Vec<(i64, Sponsorship)> = rows
            .into_iter()
            .filter_map(|(r, row)| r.sponsorship.map(|s| (row.n1.unwrap_or(0), s)))
            .filter(|(_, s)| {
                (include_ended
                    || !matches!(
                        state_of(s),
                        SponsorshipState::Ended | SponsorshipState::Failed
                    ))
                    && (!s.private
                        || (include_private && parties.iter().any(|p| Self::is_party(s, p))))
            })
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.id.cmp(&a.1.id)));
        page_vec(v.into_iter().map(|(_, s)| s).collect(), token, limit)
    }

    // ---- sponsor-only access ----------------------------------------------------------

    /// Who may read which sponsor-only repos through live sponsorships of `maintainee`.
    pub async fn content_access(
        &self,
        cx: &Cx,
        maintainee: &Owner,
        repo_id: &str,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<SponsorContentAccess>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let tiers = Self::tiers_of(&mut tx, &maintainee.key).await?;
        let mut out = Vec::new();
        for s in Self::sponsorships_of(&mut tx, &maintainee.key).await? {
            if !grants_access(&s, now) {
                continue;
            }
            let Some(t) = tiers.iter().find(|t| t.id == s.tier_id) else {
                continue;
            };
            let repos: Vec<RepoRef> = t
                .sponsor_only_repos
                .iter()
                .filter(|r| repo_id.is_empty() || ref_repo_id(&Some((*r).clone())) == repo_id)
                .cloned()
                .collect();
            if repos.is_empty() {
                continue;
            }
            out.push(SponsorContentAccess {
                sponsor: s.sponsor.clone(),
                tier_id: s.tier_id.clone(),
                repos,
                granted_at: s.created_at,
                expires_at: s.current_period_end_at,
            });
        }
        out.sort_by(|a, b| {
            a.sponsor
                .as_ref()
                .map(|p| &p.id)
                .cmp(&b.sponsor.as_ref().map(|p| &p.id))
                .then(a.tier_id.cmp(&b.tier_id))
        });
        page_vec(out, token, limit)
    }

    /// The tier a principal's live sponsorship grants on a repo, if any.
    pub async fn grants(
        &self,
        cx: &Cx,
        repo_id: &str,
        principal_id: &str,
    ) -> Result<Option<String>> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let rows = tx
            .scan_msgs::<SponsorshipRec>(
                &SPONSORSHIPS,
                &Scan {
                    scope: None,
                    key: Some(Key::K1(format!("user:{principal_id}"))),
                    limit: 1000,
                    ..Scan::default()
                },
            )
            .await?;
        for (rec, row) in rows {
            let Some(s) = rec.sponsorship else { continue };
            if !grants_access(&s, now) || s.tier_id.is_empty() {
                continue;
            }
            if let Some((t, _)) = tx
                .get_msg::<SponsorTier>(&TIERS, &row.scope, &s.tier_id)
                .await?
                && t.sponsor_only_repos
                    .iter()
                    .any(|r| ref_repo_id(&Some(r.clone())) == repo_id)
            {
                return Ok(Some(t.id));
            }
        }
        Ok(None)
    }

    // ---- payouts ----------------------------------------------------------------------

    /// Records a payout (scheduled, paid or failed) for an owner; the billing worker calls
    /// this.
    pub async fn record_payout(
        &self,
        cx: &Cx,
        owner: &Owner,
        amount_cents: u64,
        status: &str,
        at_ms: u64,
    ) -> Result<SponsorPayout> {
        if !["scheduled", "paid", "failed"].contains(&status) {
            return Err(ForgeError::invalid(
                "invalid_status",
                "payout status is scheduled, paid or failed",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let p = SponsorPayout {
            id: tx.new_id()?,
            amount_cents,
            status: status.to_string(),
            paid_at: (status == "paid").then(|| ts(at_ms)),
        };
        tx.insert(
            &PAYOUTS,
            &Row::new(owner.key.clone(), p.id.clone())
                .n1(at_ms as i64)
                .msg(&p),
        )
        .await?;
        Self::announce(
            &mut tx,
            "sponsor.payout_recorded",
            "sponsor.payout_record",
            &owner.principal.id,
            serde_json::json!({"owner": owner.key, "payout": &p}),
        )
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    /// Payout totals and the most recent payouts.
    pub async fn payout_summary(&self, cx: &Cx, owner: &Owner) -> Result<SponsorPayoutSummary> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (rec, _) = Self::rec(&mut tx, owner).await?;
        let rows = tx
            .scan_msgs::<SponsorPayout>(
                &PAYOUTS,
                &Scan::scope(owner.key.clone(), 10_000).by_n1().desc(),
            )
            .await?;
        let (mut pending, mut last30, mut life) = (0u64, 0u64, 0u64);
        let mut next: Option<u64> = None;
        let mut recent = Vec::new();
        for (p, row) in &rows {
            let at = row.n1.unwrap_or(0) as u64;
            match p.status.as_str() {
                "scheduled" => {
                    pending += p.amount_cents;
                    next = Some(next.map_or(at, |n| n.min(at)));
                }
                "paid" => {
                    life += p.amount_cents;
                    if at + 30 * DAY_MS >= now {
                        last30 += p.amount_cents;
                    }
                }
                _ => {}
            }
            if recent.len() < 10 {
                recent.push(p.clone());
            }
        }
        Ok(SponsorPayoutSummary {
            maintainee: Some(owner.principal.clone()),
            currency: rec
                .profile
                .map(|p| p.currency)
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "USD".into()),
            pending_cents: pending,
            paid_last_30_days_cents: last30,
            paid_lifetime_cents: life,
            next_payout_at: next.map(ts),
            recent_payouts: recent,
        })
    }

    // ---- FUNDING.yml ------------------------------------------------------------------

    /// The repo's `FUNDING.yml` (`.github/`, root or `docs/`) at a revision, parsed.
    pub async fn funding_file(&self, repo: &Repo, at: &ObjectId) -> Result<SponsorFundingFile> {
        let vr = VcsRepo::of_repo(repo);
        for path in [
            ".github/FUNDING.yml",
            ".github/FUNDING.yaml",
            "FUNDING.yml",
            "docs/FUNDING.yml",
        ] {
            if let Some((blob, bytes)) = read_file(self.vcs.as_ref(), &vr, at, path).await? {
                let (links, warnings) =
                    parse_funding_with_base(&String::from_utf8_lossy(&bytes), &self.base_url)?;
                return Ok(SponsorFundingFile {
                    repo: repo_ref_id(&repo.id),
                    path: path.as_bytes().to_vec(),
                    blob: Some(blob),
                    links,
                    warnings,
                });
            }
        }
        Err(ForgeError::not_found("FUNDING.yml"))
    }
}

/// Grants `read` on sponsor-only repos to principals with a live sponsorship of the tier.
pub struct SponsorAccess(pub Arc<Sponsors>);

#[async_trait]
impl AccessSource for SponsorAccess {
    async fn grants(
        &self,
        cx: &Cx,
        repo: &Repo,
        principal_id: &str,
    ) -> Result<Vec<(RepoRole, String)>> {
        match self.0.grants(cx, &repo.id, principal_id).await {
            Ok(Some(tier)) => Ok(vec![(RepoRole::Read, format!("sponsor:{tier}"))]),
            Ok(None) => Ok(Vec::new()),
            // Sponsor data being down must not lock out everyone else's access checks.
            Err(e) if e.retryable() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}
