//! The marketplace: listings, plans, purchases and publishers.
//!
//! # Listing states
//!
//! `DRAFT -> PENDING_REVIEW -> PUBLISHED | REJECTED`, `PUBLISHED -> UNPUBLISHED`; a rejected
//! or unpublished listing can be submitted again. The publisher org's owners, admins and app
//! managers submit; with [`super::MarketplaceConfig::auto_approve`] (the self-hosted default)
//! submitting publishes at once, otherwise a site admin decides with
//! [`Apps::review_listing`]. Only published listings are visible to everyone; the publisher
//! also sees the rest.
//!
//! # Plans and purchases
//!
//! Prices are micro-USD. A plan that has any purchase can only be retired, never repriced.
//! A purchase is per org and listing: buying again changes the plan. Billing cycles are
//! 30 days (monthly) and 365 days (yearly) from the injected clock; a first purchase with a
//! free trial bills at the end of the trial. Cancelling ends the purchase at the end of the
//! current cycle (`pending_cancellation`, then `cancelled_at` once `next_billing_at`
//! passes; read lazily from the clock). Paid purchases consult the optional
//! [`crate::enterprise::common::SpendGate`] and surface `QUOTA_EXCEEDED`.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bee_proto::v1::{
    BillingProduct, ErrorReason, MarketplaceBillingCycle, MarketplaceListing,
    MarketplaceListingState, MarketplaceListingType, MarketplacePlan, MarketplacePricingModel,
    MarketplacePublisher, MarketplacePublisherVerification, MarketplacePurchase, RepoRef,
    SubjectKind, repo_ref,
};

use super::registry::{load_app, page_rows};
use super::{
    Account, Apps, DomainCheck, INSTALLS, LISTINGS, ListingRec, PLANS, PUBLISHERS, PURCHASES,
    PlanRec, PublisherRec, PurchaseRec, SURFACE, tsm,
};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::enterprise::common::{emit, get_or_404, json, scan_all, valid_slug};
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::ids::ts_ms;
use crate::store::{Key, Row, Scan, Tx};

/// Most listings one publisher may have.
pub const MAX_LISTINGS_PER_PUBLISHER: u64 = 200;
/// Most plans one listing may have.
pub const MAX_PLANS_PER_LISTING: u64 = 10;
/// Length of a monthly cycle.
pub const MONTH_MS: u64 = 30 * 24 * 3600 * 1000;
/// Length of a yearly cycle.
pub const YEAR_MS: u64 = 365 * 24 * 3600 * 1000;
const DAY_MS: u64 = 24 * 3600 * 1000;

/// A [`DomainCheck`] over the org's own `verified_domains`; two-factor enforcement is assumed
/// (the enterprise domain supplies the real answer when wired).
pub struct OrgDomainCheck {
    forge: std::sync::Arc<Forge>,
}

impl OrgDomainCheck {
    /// A check over `forge`'s orgs.
    pub fn new(forge: std::sync::Arc<Forge>) -> OrgDomainCheck {
        OrgDomainCheck { forge }
    }
}

#[async_trait]
impl DomainCheck for OrgDomainCheck {
    async fn is_verified(&self, cx: &Cx, org_id: &str, domain: &str) -> Result<bool> {
        let org = self.forge.orgs.org_any(cx, org_id).await?;
        Ok(org
            .verified_domains
            .iter()
            .any(|d| d.eq_ignore_ascii_case(domain)))
    }

    async fn two_factor_required(&self, _cx: &Cx, _org_id: &str) -> Result<bool> {
        Ok(true)
    }
}

/// A new listing.
#[derive(Clone, Debug, Default)]
pub struct ListingInput {
    /// What it offers.
    pub listing_type: MarketplaceListingType,
    /// URL slug.
    pub slug: String,
    /// Name.
    pub name: String,
    /// One line.
    pub short_description: String,
    /// Markdown.
    pub full_description: String,
    /// Categories (slugs).
    pub categories: Vec<String>,
    /// APP listings: the app (id, slug or client id).
    pub app_id: String,
    /// ACTION listings: the repo id (resolved by the caller; it must belong to the publisher).
    pub action_repo_id: String,
    /// ACTION listings: path of the action inside the repo.
    pub action_path: String,
    /// AGENT listings: the agent definition id.
    pub agent_id: String,
    /// Logo.
    pub logo_url: String,
    /// Support page.
    pub support_url: String,
    /// Privacy policy.
    pub privacy_policy_url: String,
    /// Terms of service.
    pub terms_url: String,
}

/// A partial listing update.
#[derive(Clone, Debug, Default)]
pub struct ListingPatch {
    /// Name.
    pub name: Option<String>,
    /// One line.
    pub short_description: Option<String>,
    /// Markdown.
    pub full_description: Option<String>,
    /// Categories (replaces the list).
    pub categories: Option<Vec<String>>,
    /// Logo.
    pub logo_url: Option<String>,
    /// Screenshots (replaces the list).
    pub screenshot_urls: Option<Vec<String>>,
    /// Support page.
    pub support_url: Option<String>,
    /// Privacy policy.
    pub privacy_policy_url: Option<String>,
    /// Terms of service.
    pub terms_url: Option<String>,
}

/// A new plan.
#[derive(Clone, Debug, Default)]
pub struct PlanInput {
    /// Name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Pricing model.
    pub pricing_model: MarketplacePricingModel,
    /// Monthly price (micro-USD; per unit for PER_UNIT).
    pub monthly_price_micro_usd: u64,
    /// Yearly price.
    pub yearly_price_micro_usd: u64,
    /// PER_UNIT: what a unit is.
    pub unit_name: String,
    /// Trial length.
    pub free_trial_days: u32,
    /// Feature list.
    pub bullets: Vec<String>,
}

/// A partial plan update.
#[derive(Clone, Debug, Default)]
pub struct PlanPatch {
    /// Name.
    pub name: Option<String>,
    /// Description.
    pub description: Option<String>,
    /// Monthly price (`INVALID_STATE` once the plan has purchases).
    pub monthly_price_micro_usd: Option<u64>,
    /// Yearly price (`INVALID_STATE` once the plan has purchases).
    pub yearly_price_micro_usd: Option<u64>,
    /// Trial length for future purchases.
    pub free_trial_days: Option<u32>,
    /// Feature list (replaces).
    pub bullets: Option<Vec<String>>,
    /// Retire or reinstate.
    pub retired: Option<bool>,
}

/// A purchase request.
#[derive(Clone, Debug, Default)]
pub struct PurchaseInput {
    /// The plan.
    pub plan_id: String,
    /// Units (PER_UNIT plans; other plans use 1).
    pub unit_count: u32,
    /// Billing cycle (paid plans; unspecified = monthly).
    pub billing_cycle: MarketplaceBillingCycle,
}

/// Filters of a listing search.
#[derive(Clone, Debug, Default)]
pub struct ListingFilter {
    /// Free text over name, slug, descriptions and categories (every word must match).
    pub query: String,
    /// Type (unspecified = all).
    pub listing_type: MarketplaceListingType,
    /// Category.
    pub category: String,
    /// Only this publisher org (id).
    pub publisher_org_id: String,
    /// Only listings of verified publishers.
    pub verified_only: bool,
    /// Only listings that cost nothing.
    pub free_only: bool,
    /// `popularity` (default), `recent` or `name`.
    pub sort: String,
    /// Also show the unpublished listings of this org (id), the caller's own.
    pub include_unpublished_of: String,
}

/// A listing with its publisher org id (the proto carries the name).
#[derive(Clone, Debug)]
pub struct ListingView {
    /// The listing as served.
    pub listing: MarketplaceListing,
    /// Publisher org id.
    pub publisher_org_id: String,
}

fn bad(sub: &str, m: impl Into<String>) -> ForgeError {
    ForgeError::invalid(sub, m)
}

fn check_text(field: &str, s: &str, max: usize) -> Result<()> {
    if s.chars().count() > max {
        return Err(bad(
            "too_long",
            format!("`{field}` is longer than {max} characters"),
        ));
    }
    Ok(())
}

fn check_optional_url(field: &str, u: &str) -> Result<()> {
    if u.is_empty() {
        Ok(())
    } else {
        super::registry::check_url(field, u, "bad_url")
    }
}

fn check_categories(c: &[String]) -> Result<Vec<String>> {
    if c.len() > 5 {
        return Err(bad("too_many_categories", "at most 5 categories"));
    }
    let mut out: Vec<String> = Vec::new();
    for x in c {
        let x = x.trim().to_lowercase();
        if !valid_slug(&x) || x.len() > 40 {
            return Err(bad("invalid_category", format!("bad category {x:?}")));
        }
        if !out.contains(&x) {
            out.push(x);
        }
    }
    Ok(out)
}

fn check_plan(p: &MarketplacePlan) -> Result<()> {
    if p.name.trim().is_empty() || p.name.chars().count() > 60 {
        return Err(bad("invalid_name", "plan name is 1-60 characters"));
    }
    check_text("description", &p.description, 500)?;
    if p.bullets.len() > 10 || p.bullets.iter().any(|b| b.chars().count() > 120) {
        return Err(bad("bad_bullets", "at most 10 bullets of 120 characters"));
    }
    if p.free_trial_days > 90 {
        return Err(bad("bad_trial", "free_trial_days is at most 90"));
    }
    let model = MarketplacePricingModel::try_from(p.pricing_model)
        .unwrap_or(MarketplacePricingModel::Unspecified);
    match model {
        MarketplacePricingModel::Free => {
            if p.monthly_price_micro_usd != 0
                || p.yearly_price_micro_usd != 0
                || !p.unit_name.is_empty()
            {
                return Err(bad("bad_price", "a FREE plan has no prices or unit"));
            }
            if p.free_trial_days != 0 {
                return Err(bad("bad_trial", "a FREE plan has no trial"));
            }
        }
        MarketplacePricingModel::Flat => {
            if p.monthly_price_micro_usd == 0 && p.yearly_price_micro_usd == 0 {
                return Err(bad(
                    "bad_price",
                    "a FLAT plan needs a monthly or yearly price",
                ));
            }
            if !p.unit_name.is_empty() {
                return Err(bad("bad_price", "only PER_UNIT plans have a unit"));
            }
        }
        MarketplacePricingModel::PerUnit => {
            if p.monthly_price_micro_usd == 0 && p.yearly_price_micro_usd == 0 {
                return Err(bad(
                    "bad_price",
                    "a PER_UNIT plan needs a monthly or yearly price",
                ));
            }
            if p.unit_name.trim().is_empty() {
                return Err(bad(
                    "missing_field",
                    "`unit_name` is required for PER_UNIT plans",
                ));
            }
        }
        MarketplacePricingModel::Unspecified => {
            return Err(bad("missing_field", "`pricing_model` is required"));
        }
    }
    Ok(())
}

fn state_of(rec: &ListingRec) -> MarketplaceListingState {
    rec.listing
        .as_ref()
        .and_then(|l| MarketplaceListingState::try_from(l.state).ok())
        .unwrap_or(MarketplaceListingState::Unspecified)
}

async fn load_listing(tx: &mut Tx, by: &str) -> Result<(ListingRec, Row)> {
    if by.is_empty() {
        return Err(ForgeError::missing("listing"));
    }
    if let Some(x) = tx.get_msg::<ListingRec>(&LISTINGS, "", by).await? {
        return Ok(x);
    }
    let rows = tx
        .scan(&LISTINGS, &Scan::scope("", 1).key(Key::K1(by.to_string())))
        .await?;
    match rows.into_iter().next() {
        Some(row) => Ok((row.decode()?, row)),
        None => Err(ForgeError::not_found(format!("listing {by}"))),
    }
}

async fn plans_of(tx: &mut Tx, listing_id: &str) -> Result<Vec<MarketplacePlan>> {
    Ok(scan_all::<PlanRec>(tx, &PLANS, listing_id, None)
        .await?
        .into_iter()
        .filter_map(|(p, _)| p.plan)
        .collect())
}

async fn publisher_verified(tx: &mut Tx, org_id: &str) -> Result<bool> {
    Ok(tx
        .get_msg::<PublisherRec>(&PUBLISHERS, "", org_id)
        .await?
        .and_then(|(p, _)| p.publisher)
        .is_some_and(|p| p.verification == MarketplacePublisherVerification::Verified as i32))
}

async fn listing_view(tx: &mut Tx, rec: &ListingRec) -> Result<MarketplaceListing> {
    let mut l = rec.listing.clone().unwrap_or_default();
    l.plans = plans_of(tx, &l.id).await?;
    let mut installs = rec.purchase_count;
    if !l.app_id.is_empty() {
        installs += tx
            .count(&INSTALLS, "", Some(&Key::K1(l.app_id.clone())))
            .await?;
    }
    l.install_count = installs;
    l.verified_publisher = publisher_verified(tx, &rec.publisher_org_id).await?;
    Ok(l)
}

fn effective_purchase(p: &MarketplacePurchase, now: u64) -> MarketplacePurchase {
    let mut p = p.clone();
    if p.on_free_trial && p.free_trial_ends_at.as_ref().map(ts_ms).unwrap_or(0) <= now {
        p.on_free_trial = false;
    }
    if p.pending_cancellation && p.cancelled_at.is_none() {
        let end = p.next_billing_at.as_ref().map(ts_ms).unwrap_or(now);
        if end <= now {
            p.cancelled_at = tsm(end);
            p.pending_cancellation = false;
        }
    }
    p
}

fn is_free(plans: &[MarketplacePlan]) -> bool {
    plans.is_empty()
        || plans
            .iter()
            .any(|p| !p.retired && p.pricing_model == MarketplacePricingModel::Free as i32)
}

impl Apps {
    async fn org_name(&self, cx: &Cx, org_id: &str) -> String {
        self.account_name(cx, &format!("org:{org_id}")).await
    }

    async fn name_listing(&self, cx: &Cx, v: &mut ListingView) {
        v.listing.publisher = self.org_name(cx, &v.publisher_org_id).await;
    }

    // ---------- listings ----------

    /// Creates a draft listing for the publisher org.
    pub async fn create_listing(
        &self,
        cx: &Cx,
        publisher: &Account,
        input: ListingInput,
    ) -> Result<ListingView> {
        if !valid_slug(&input.slug) || input.slug.len() > 64 {
            return Err(bad(
                "invalid_slug",
                "slug is 1-64 lower-case letters, digits, `-`, `_`, `.`",
            ));
        }
        if input.name.trim().is_empty() || input.name.chars().count() > 100 {
            return Err(bad("invalid_name", "name is 1-100 characters"));
        }
        if input.short_description.trim().is_empty() {
            return Err(ForgeError::missing("short_description"));
        }
        check_text("short_description", &input.short_description, 200)?;
        check_text("full_description", &input.full_description, 20_000)?;
        let categories = check_categories(&input.categories)?;
        for (f, u) in [
            ("logo_url", &input.logo_url),
            ("support_url", &input.support_url),
            ("privacy_policy_url", &input.privacy_policy_url),
            ("terms_url", &input.terms_url),
        ] {
            check_optional_url(f, u)?;
        }
        let ty = input.listing_type;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let mut app_id = String::new();
        let mut action_repo = None;
        let mut action_path = String::new();
        match ty {
            MarketplaceListingType::App => {
                if input.app_id.is_empty() {
                    return Err(ForgeError::missing("app_id"));
                }
                let (app, _) = load_app(&mut tx, &input.app_id).await?;
                if app.owner_key != publisher.key() {
                    return Err(bad(
                        "app_not_owned",
                        "the app is not owned by the publisher org",
                    ));
                }
                app_id = app.app.map(|a| a.id).unwrap_or_default();
            }
            MarketplaceListingType::Action => {
                if input.action_repo_id.is_empty() {
                    return Err(ForgeError::missing("action_repo"));
                }
                action_repo = Some(RepoRef {
                    repo: Some(repo_ref::Repo::Id(input.action_repo_id.clone())),
                });
                action_path = if input.action_path.is_empty() {
                    "action.yml".to_string()
                } else {
                    input.action_path.clone()
                };
            }
            MarketplaceListingType::Agent => {
                if input.agent_id.is_empty() {
                    return Err(ForgeError::missing("agent_id"));
                }
            }
            MarketplaceListingType::Unspecified => return Err(ForgeError::missing("type")),
        }
        if !tx
            .scan(
                &LISTINGS,
                &Scan::scope("", 1).key(Key::K1(input.slug.clone())),
            )
            .await?
            .is_empty()
        {
            return Err(ForgeError::already_exists(format!(
                "listing {}",
                input.slug
            )));
        }
        let have = tx
            .count(&LISTINGS, "", Some(&Key::K2(publisher.id.clone())))
            .await?;
        if have >= MAX_LISTINGS_PER_PUBLISHER {
            return Err(ForgeError::too_large(
                "listings_per_publisher",
                MAX_LISTINGS_PER_PUBLISHER,
                have + 1,
            ));
        }
        let id = tx.new_id()?;
        let listing = MarketplaceListing {
            id: id.clone(),
            slug: input.slug.clone(),
            r#type: ty as i32,
            name: input.name.trim().to_string(),
            short_description: input.short_description,
            full_description: input.full_description,
            categories,
            publisher: publisher.name.clone(),
            verified_publisher: false,
            app_id,
            action_repo,
            action_path,
            agent_id: input.agent_id,
            logo_url: input.logo_url,
            screenshot_urls: Vec::new(),
            support_url: input.support_url,
            privacy_policy_url: input.privacy_policy_url,
            terms_url: input.terms_url,
            state: MarketplaceListingState::Draft as i32,
            plans: Vec::new(),
            install_count: 0,
            created_at: tsm(now),
            updated_at: tsm(now),
            published_at: None,
        };
        let rec = ListingRec {
            listing: Some(listing.clone()),
            publisher_org_id: publisher.id.clone(),
            purchase_count: 0,
            review_note: String::new(),
        };
        tx.insert(
            &LISTINGS,
            &Row::new("", id.clone())
                .k1(input.slug)
                .k2(publisher.id.clone())
                .n1(now as i64)
                .msg(&rec),
        )
        .await?;
        tx.audit(
            AuditDraft::new("marketplace.listing_create", &publisher.id)
                .target_id(&id)
                .after(&json(&listing)),
        )
        .await?;
        let view = listing_view(&mut tx, &rec).await?;
        tx.commit().await?;
        Ok(ListingView {
            listing: view,
            publisher_org_id: publisher.id.clone(),
        })
    }

    /// Changes a listing's metadata.
    pub async fn update_listing(&self, cx: &Cx, by: &str, p: ListingPatch) -> Result<ListingView> {
        if let Some(n) = &p.name
            && (n.trim().is_empty() || n.chars().count() > 100)
        {
            return Err(bad("invalid_name", "name is 1-100 characters"));
        }
        if let Some(s) = &p.short_description {
            if s.trim().is_empty() {
                return Err(ForgeError::missing("short_description"));
            }
            check_text("short_description", s, 200)?;
        }
        if let Some(s) = &p.full_description {
            check_text("full_description", s, 20_000)?;
        }
        let categories = match &p.categories {
            Some(c) => Some(check_categories(c)?),
            None => None,
        };
        for (f, u) in [
            ("logo_url", &p.logo_url),
            ("support_url", &p.support_url),
            ("privacy_policy_url", &p.privacy_policy_url),
            ("terms_url", &p.terms_url),
        ] {
            if let Some(u) = u {
                check_optional_url(f, u)?;
            }
        }
        if let Some(s) = &p.screenshot_urls {
            if s.len() > 10 {
                return Err(bad("too_many_screenshots", "at most 10 screenshots"));
            }
            for u in s {
                super::registry::check_url("screenshot_urls", u, "bad_url")?;
            }
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (mut rec, row) = load_listing(&mut tx, by).await?;
        let mut l = rec.listing.clone().unwrap_or_default();
        let before = json(&l);
        if let Some(v) = p.name {
            l.name = v.trim().to_string();
        }
        if let Some(v) = p.short_description {
            l.short_description = v;
        }
        if let Some(v) = p.full_description {
            l.full_description = v;
        }
        if let Some(v) = categories {
            l.categories = v;
        }
        if let Some(v) = p.logo_url {
            l.logo_url = v;
        }
        if let Some(v) = p.screenshot_urls {
            l.screenshot_urls = v;
        }
        if let Some(v) = p.support_url {
            l.support_url = v;
        }
        if let Some(v) = p.privacy_policy_url {
            l.privacy_policy_url = v;
        }
        if let Some(v) = p.terms_url {
            l.terms_url = v;
        }
        l.updated_at = tsm(now);
        rec.listing = Some(l.clone());
        tx.update(&LISTINGS, &row.clone().msg(&rec)).await?;
        tx.audit(
            AuditDraft::new("marketplace.listing_update", &rec.publisher_org_id)
                .target_id(&l.id)
                .before(&before)
                .after(&json(&l)),
        )
        .await?;
        let view = listing_view(&mut tx, &rec).await?;
        tx.commit().await?;
        let mut v = ListingView {
            listing: view,
            publisher_org_id: rec.publisher_org_id,
        };
        self.name_listing(cx, &mut v).await;
        Ok(v)
    }

    async fn transition(
        &self,
        cx: &Cx,
        by: &str,
        action: &str,
        f: impl FnOnce(MarketplaceListingState) -> Result<(MarketplaceListingState, String)>,
    ) -> Result<ListingView> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (mut rec, row) = load_listing(&mut tx, by).await?;
        let from = state_of(&rec);
        let (to, note) = f(from)?;
        let mut l = rec.listing.clone().unwrap_or_default();
        l.state = to as i32;
        l.updated_at = tsm(now);
        if to == MarketplaceListingState::Published {
            l.published_at = tsm(now);
        }
        rec.listing = Some(l.clone());
        if !note.is_empty() {
            rec.review_note = note.clone();
        }
        tx.update(&LISTINGS, &row.clone().msg(&rec)).await?;
        tx.audit(
            AuditDraft::new(action, &rec.publisher_org_id)
                .target_id(&l.id)
                .before(&format!("{{\"state\":\"{}\"}}", from.as_str_name()))
                .after(&serde_json::json!({ "state": to.as_str_name(), "note": note }).to_string()),
        )
        .await?;
        let view = listing_view(&mut tx, &rec).await?;
        tx.commit().await?;
        let mut v = ListingView {
            listing: view,
            publisher_org_id: rec.publisher_org_id,
        };
        self.name_listing(cx, &mut v).await;
        Ok(v)
    }

    /// Submits a listing for review (publishing it at once with `auto_approve`), or, with
    /// `unpublish`, takes a published listing down (existing purchases continue).
    pub async fn publish_listing(&self, cx: &Cx, by: &str, unpublish: bool) -> Result<ListingView> {
        use MarketplaceListingState as S;
        let auto = self.cfg.marketplace.auto_approve;
        self.transition(
            cx,
            by,
            if unpublish {
                "marketplace.unpublish"
            } else {
                "marketplace.submit"
            },
            move |from| {
                if unpublish {
                    return match from {
                        S::Published => Ok((S::Unpublished, String::new())),
                        other => Err(ForgeError::invalid_state(format!(
                            "only a published listing can be unpublished, this one is {}",
                            other.as_str_name()
                        ))),
                    };
                }
                match from {
                    S::Draft | S::Rejected | S::Unpublished => Ok((
                        if auto { S::Published } else { S::PendingReview },
                        String::new(),
                    )),
                    other => Err(ForgeError::invalid_state(format!(
                        "a {} listing cannot be submitted",
                        other.as_str_name()
                    ))),
                }
            },
        )
        .await
    }

    /// A site admin's decision on a listing waiting in `PENDING_REVIEW`.
    pub async fn review_listing(
        &self,
        cx: &Cx,
        by: &str,
        approve: bool,
        note: &str,
    ) -> Result<ListingView> {
        use MarketplaceListingState as S;
        let note = note.to_string();
        self.transition(cx, by, "marketplace.review", move |from| match from {
            S::PendingReview => Ok((if approve { S::Published } else { S::Rejected }, note)),
            other => Err(ForgeError::invalid_state(format!(
                "only a listing pending review can be reviewed, this one is {}",
                other.as_str_name()
            ))),
        })
        .await
    }

    /// Reads a listing by slug or id, whatever its state (the service hides non-published ones
    /// from outsiders).
    pub async fn get_listing(&self, cx: &Cx, by: &str) -> Result<ListingView> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_listing(&mut tx, by).await?;
        let listing = listing_view(&mut tx, &rec).await?;
        drop(tx);
        let mut v = ListingView {
            listing,
            publisher_org_id: rec.publisher_org_id,
        };
        self.name_listing(cx, &mut v).await;
        Ok(v)
    }

    /// Lists or searches listings; the page token is an offset into the sorted result.
    pub async fn list_listings(
        &self,
        cx: &Cx,
        f: &ListingFilter,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ListingView>, String)> {
        let sort = match f.sort.as_str() {
            "" | "popularity" => "popularity",
            "recent" => "recent",
            "name" => "name",
            other => return Err(bad("bad_sort", format!("unknown sort {other:?}"))),
        };
        let offset: usize = if after.is_empty() {
            0
        } else {
            after
                .parse()
                .map_err(|_| bad("bad_page_token", "malformed page token"))?
        };
        let words: Vec<String> = f
            .query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let category = f.category.trim().to_lowercase();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let all = scan_all::<ListingRec>(&mut tx, &LISTINGS, "", None).await?;
        let mut verified: BTreeMap<String, bool> = BTreeMap::new();
        let mut hits: Vec<(ListingRec, MarketplaceListing)> = Vec::new();
        for (rec, _) in all {
            let visible = state_of(&rec) == MarketplaceListingState::Published
                || (!f.include_unpublished_of.is_empty()
                    && rec.publisher_org_id == f.include_unpublished_of);
            if !visible
                || (!f.publisher_org_id.is_empty() && rec.publisher_org_id != f.publisher_org_id)
            {
                continue;
            }
            let l = rec.listing.clone().unwrap_or_default();
            if f.listing_type != MarketplaceListingType::Unspecified
                && l.r#type != f.listing_type as i32
            {
                continue;
            }
            if !category.is_empty() && !l.categories.contains(&category) {
                continue;
            }
            if !words.is_empty() {
                let hay = format!(
                    "{} {} {} {} {}",
                    l.name,
                    l.slug,
                    l.short_description,
                    l.full_description,
                    l.categories.join(" ")
                )
                .to_lowercase();
                if !words.iter().all(|w| hay.contains(w.as_str())) {
                    continue;
                }
            }
            let v = match verified.get(&rec.publisher_org_id) {
                Some(v) => *v,
                None => {
                    let v = publisher_verified(&mut tx, &rec.publisher_org_id).await?;
                    verified.insert(rec.publisher_org_id.clone(), v);
                    v
                }
            };
            if f.verified_only && !v {
                continue;
            }
            let view = listing_view(&mut tx, &rec).await?;
            if f.free_only && !is_free(&view.plans) {
                continue;
            }
            hits.push((rec, view));
        }
        drop(tx);
        match sort {
            "name" => hits.sort_by(|a, b| {
                a.1.name
                    .to_lowercase()
                    .cmp(&b.1.name.to_lowercase())
                    .then(a.1.id.cmp(&b.1.id))
            }),
            "recent" => hits.sort_by(|a, b| {
                let t = |l: &MarketplaceListing| {
                    l.published_at
                        .as_ref()
                        .or(l.created_at.as_ref())
                        .map(ts_ms)
                        .unwrap_or(0)
                };
                t(&b.1).cmp(&t(&a.1)).then(b.1.id.cmp(&a.1.id))
            }),
            _ => hits.sort_by(|a, b| {
                b.1.install_count
                    .cmp(&a.1.install_count)
                    .then(a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
                    .then(a.1.id.cmp(&b.1.id))
            }),
        }
        let limit = limit.clamp(1, 500);
        let total = hits.len();
        let page: Vec<(ListingRec, MarketplaceListing)> =
            hits.into_iter().skip(offset).take(limit).collect();
        let next = if offset + page.len() < total {
            (offset + page.len()).to_string()
        } else {
            String::new()
        };
        let mut out = Vec::new();
        for (rec, listing) in page {
            let mut v = ListingView {
                listing,
                publisher_org_id: rec.publisher_org_id,
            };
            self.name_listing(cx, &mut v).await;
            out.push(v);
        }
        Ok((out, next))
    }

    // ---------- plans ----------

    /// Adds a pricing plan to a listing.
    pub async fn create_plan(
        &self,
        cx: &Cx,
        listing_by: &str,
        input: PlanInput,
    ) -> Result<MarketplacePlan> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (rec, _) = load_listing(&mut tx, listing_by).await?;
        let listing_id = rec
            .listing
            .as_ref()
            .map(|l| l.id.clone())
            .unwrap_or_default();
        let have = tx.count(&PLANS, &listing_id, None).await?;
        if have >= MAX_PLANS_PER_LISTING {
            return Err(ForgeError::too_large(
                "plans_per_listing",
                MAX_PLANS_PER_LISTING,
                have + 1,
            ));
        }
        let plan = MarketplacePlan {
            id: tx.new_id()?,
            listing_id: listing_id.clone(),
            name: input.name.trim().to_string(),
            description: input.description,
            pricing_model: input.pricing_model as i32,
            monthly_price_micro_usd: input.monthly_price_micro_usd,
            yearly_price_micro_usd: input.yearly_price_micro_usd,
            unit_name: input.unit_name.trim().to_string(),
            free_trial_days: input.free_trial_days,
            bullets: input.bullets,
            retired: false,
        };
        check_plan(&plan)?;
        tx.insert(
            &PLANS,
            &Row::new(listing_id.clone(), plan.id.clone())
                .n1(now as i64)
                .msg(&PlanRec {
                    plan: Some(plan.clone()),
                    created_ms: now,
                }),
        )
        .await?;
        tx.audit(
            AuditDraft::new("marketplace.plan_create", &rec.publisher_org_id)
                .target_id(&plan.id)
                .after(&json(&plan)),
        )
        .await?;
        tx.commit().await?;
        Ok(plan)
    }

    /// Changes a plan. A plan with purchases can only be retired (or have its text edited):
    /// changing a price is `INVALID_STATE`.
    pub async fn update_plan(
        &self,
        cx: &Cx,
        listing_by: &str,
        plan_id: &str,
        p: PlanPatch,
    ) -> Result<MarketplacePlan> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rec, _) = load_listing(&mut tx, listing_by).await?;
        let listing_id = rec
            .listing
            .as_ref()
            .map(|l| l.id.clone())
            .unwrap_or_default();
        let (mut prec, row) =
            get_or_404::<PlanRec>(&mut tx, &PLANS, &listing_id, plan_id, "plan").await?;
        let mut plan = prec.plan.clone().unwrap_or_default();
        let before = json(&plan);
        let reprices = p
            .monthly_price_micro_usd
            .is_some_and(|v| v != plan.monthly_price_micro_usd)
            || p.yearly_price_micro_usd
                .is_some_and(|v| v != plan.yearly_price_micro_usd);
        if reprices {
            let purchased = tx
                .scan(
                    &PURCHASES,
                    &Scan {
                        key: Some(Key::K2(plan_id.to_string())),
                        limit: 1,
                        ..Scan::default()
                    },
                )
                .await?;
            if !purchased.is_empty() {
                return Err(ForgeError::invalid_state(
                    "a plan with purchases can only be retired, not repriced",
                ));
            }
        }
        if let Some(v) = p.name {
            plan.name = v.trim().to_string();
        }
        if let Some(v) = p.description {
            plan.description = v;
        }
        if let Some(v) = p.monthly_price_micro_usd {
            plan.monthly_price_micro_usd = v;
        }
        if let Some(v) = p.yearly_price_micro_usd {
            plan.yearly_price_micro_usd = v;
        }
        if let Some(v) = p.free_trial_days {
            plan.free_trial_days = v;
        }
        if let Some(v) = p.bullets {
            plan.bullets = v;
        }
        if let Some(v) = p.retired {
            plan.retired = v;
        }
        check_plan(&plan)?;
        prec.plan = Some(plan.clone());
        tx.update(&PLANS, &row.clone().msg(&prec)).await?;
        tx.audit(
            AuditDraft::new("marketplace.plan_update", &rec.publisher_org_id)
                .target_id(plan_id)
                .before(&before)
                .after(&json(&plan)),
        )
        .await?;
        tx.commit().await?;
        Ok(plan)
    }

    // ---------- purchases ----------

    async fn name_purchase(&self, cx: &Cx, p: &mut MarketplacePurchase) {
        p.org = self.org_name(cx, &p.org).await;
    }

    /// Buys a plan for an org, or changes the org's plan on the listing. Paid plans consult
    /// the spend gate (`QUOTA_EXCEEDED`).
    pub async fn purchase_plan(
        &self,
        cx: &Cx,
        buyer: &Account,
        listing_by: &str,
        input: PurchaseInput,
    ) -> Result<MarketplacePurchase> {
        let (listing, plan, existing, had_any) = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            let (rec, _) = load_listing(&mut tx, listing_by).await?;
            if state_of(&rec) != MarketplaceListingState::Published {
                return Err(ForgeError::not_found(format!("listing {listing_by}")));
            }
            let l = rec.listing.clone().unwrap_or_default();
            let (prec, _) =
                get_or_404::<PlanRec>(&mut tx, &PLANS, &l.id, &input.plan_id, "plan").await?;
            let rows = scan_all::<PurchaseRec>(
                &mut tx,
                &PURCHASES,
                &buyer.id,
                Some(Key::K1(l.id.clone())),
            )
            .await?;
            let now = tx.now_ms();
            let had_any = !rows.is_empty();
            let existing = rows.into_iter().find(|(p, _)| {
                p.purchase
                    .as_ref()
                    .is_some_and(|p| effective_purchase(p, now).cancelled_at.is_none())
            });
            (l, prec.plan.unwrap_or_default(), existing, had_any)
        };
        if plan.retired {
            return Err(
                ForgeError::invalid_state("the plan is retired").with_subcode("plan_retired")
            );
        }
        let model = MarketplacePricingModel::try_from(plan.pricing_model)
            .unwrap_or(MarketplacePricingModel::Free);
        let units = match model {
            MarketplacePricingModel::PerUnit => {
                if input.unit_count == 0 {
                    return Err(bad("invalid_units", "`unit_count` must be at least 1"));
                }
                input.unit_count
            }
            _ => 1,
        };
        let cycle = match (model, input.billing_cycle) {
            (MarketplacePricingModel::Free, _) => MarketplaceBillingCycle::Unspecified,
            (_, MarketplaceBillingCycle::Unspecified | MarketplaceBillingCycle::Monthly) => {
                MarketplaceBillingCycle::Monthly
            }
            (_, c) => c,
        };
        let price = match cycle {
            MarketplaceBillingCycle::Monthly => plan.monthly_price_micro_usd,
            MarketplaceBillingCycle::Yearly => plan.yearly_price_micro_usd,
            MarketplaceBillingCycle::Unspecified => 0,
        };
        if model != MarketplacePricingModel::Free && price == 0 {
            return Err(bad(
                "cycle_not_offered",
                "the plan has no price for that billing cycle",
            ));
        }
        let amount = price.saturating_mul(u64::from(units));
        let new_purchase = existing.is_none();
        let trial = new_purchase && !had_any && plan.free_trial_days > 0;
        if amount > 0
            && !trial
            && let Some(g) = &self.cfg.spend_gate
        {
            g.check_spend(cx, &buyer.id, "", BillingProduct::Marketplace, amount)
                .await?;
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let cycle_ms = if cycle == MarketplaceBillingCycle::Yearly {
            YEAR_MS
        } else {
            MONTH_MS
        };
        let (lrec, lrow) = load_listing(&mut tx, &listing.id).await?;
        let purchase = match existing {
            Some((mut rec, row)) => {
                let mut p = rec.purchase.clone().unwrap_or_default();
                let before = json(&p);
                p.plan_id = plan.id.clone();
                p.unit_count = units;
                p.billing_cycle = cycle as i32;
                p.pending_cancellation = false;
                if model == MarketplacePricingModel::Free {
                    p.next_billing_at = None;
                    p.on_free_trial = false;
                } else if p.next_billing_at.is_none() {
                    // Moving from a free plan to a paid one starts billing now.
                    p.next_billing_at = tsm(now + cycle_ms);
                }
                rec.purchase = Some(p.clone());
                tx.update(&PURCHASES, &row.clone().k2(plan.id.clone()).msg(&rec))
                    .await?;
                tx.audit(
                    AuditDraft::new("marketplace.purchase_change", &buyer.id)
                        .target_id(&p.id)
                        .before(&before)
                        .after(&json(&p)),
                )
                .await?;
                p
            }
            None => {
                let id = tx.new_id()?;
                let trial_end = now + u64::from(plan.free_trial_days) * DAY_MS;
                let p = MarketplacePurchase {
                    id: id.clone(),
                    listing_id: listing.id.clone(),
                    plan_id: plan.id.clone(),
                    org: buyer.id.clone(),
                    unit_count: units,
                    billing_cycle: cycle as i32,
                    on_free_trial: trial,
                    free_trial_ends_at: if trial { tsm(trial_end) } else { None },
                    next_billing_at: match (model, trial) {
                        (MarketplacePricingModel::Free, _) => None,
                        (_, true) => tsm(trial_end),
                        _ => tsm(now + cycle_ms),
                    },
                    pending_cancellation: false,
                    purchased_by: Some(cx.principal()),
                    created_at: tsm(now),
                    cancelled_at: None,
                };
                tx.insert(
                    &PURCHASES,
                    &Row::new(buyer.id.clone(), id.clone())
                        .k1(listing.id.clone())
                        .k2(plan.id.clone())
                        .n1(now as i64)
                        .msg(&PurchaseRec {
                            purchase: Some(p.clone()),
                        }),
                )
                .await?;
                let mut lrec2 = lrec.clone();
                lrec2.purchase_count += 1;
                tx.update(&LISTINGS, &lrow.clone().msg(&lrec2)).await?;
                tx.audit(
                    AuditDraft::new("marketplace.purchase", &buyer.id)
                        .target_id(&id)
                        .after(&json(&p)),
                )
                .await?;
                p
            }
        };
        emit(
            &mut tx,
            "marketplace.purchased",
            &buyer.id,
            Some((SubjectKind::Org, buyer.id.as_str())),
            json(&purchase),
        )
        .await?;
        tx.commit().await?;
        let mut out = effective_purchase(&purchase, now);
        self.name_purchase(cx, &mut out).await;
        Ok(out)
    }

    /// Cancels a purchase at the end of the current billing cycle (a free purchase ends at
    /// once).
    pub async fn cancel_purchase(
        &self,
        cx: &Cx,
        buyer_org_id: &str,
        purchase_id: &str,
    ) -> Result<MarketplacePurchase> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (mut rec, row) =
            get_or_404::<PurchaseRec>(&mut tx, &PURCHASES, buyer_org_id, purchase_id, "purchase")
                .await?;
        let mut p = rec.purchase.clone().unwrap_or_default();
        let current = effective_purchase(&p, now);
        if current.cancelled_at.is_some() {
            return Err(ForgeError::invalid_state(
                "the purchase is already cancelled",
            ));
        }
        if p.pending_cancellation {
            return Err(ForgeError::invalid_state(
                "the purchase is already set to cancel at the end of the cycle",
            ));
        }
        let before = json(&p);
        if p.next_billing_at.is_none() {
            p.cancelled_at = tsm(now);
        } else {
            p.pending_cancellation = true;
        }
        rec.purchase = Some(p.clone());
        tx.update(&PURCHASES, &row.clone().msg(&rec)).await?;
        tx.audit(
            AuditDraft::new("marketplace.purchase_cancel", buyer_org_id)
                .target_id(purchase_id)
                .before(&before)
                .after(&json(&p)),
        )
        .await?;
        emit(
            &mut tx,
            "marketplace.cancelled",
            buyer_org_id,
            Some((SubjectKind::Org, buyer_org_id)),
            json(&p),
        )
        .await?;
        tx.commit().await?;
        let mut out = effective_purchase(&p, now);
        self.name_purchase(cx, &mut out).await;
        Ok(out)
    }

    async fn purchases_page(
        &self,
        cx: &Cx,
        scope_key: Option<(&str, Key)>,
        by_org: Option<&str>,
        include_cancelled: bool,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<MarketplacePurchase>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let (rows, next) = match (by_org, scope_key) {
            (Some(org), _) => page_rows(&mut tx, &PURCHASES, org, None, after, limit).await?,
            (None, Some((_, key))) => {
                // Across buyers: ordered by (scope, id); the cursor is `scope/id`.
                let limit = limit.clamp(1, 500);
                let (cs, ci) = after.split_once('/').unwrap_or(("", ""));
                let q = Scan {
                    key: Some(key),
                    limit: limit + 1,
                    after: (!after.is_empty()).then(|| crate::store::Cursor {
                        n1: 0,
                        scope: cs.to_string(),
                        id: ci.to_string(),
                    }),
                    ..Scan::default()
                };
                let mut rows = tx.scan(&PURCHASES, &q).await?;
                let more = rows.len() > limit;
                rows.truncate(limit);
                let next = if more {
                    rows.last()
                        .map(|r| format!("{}/{}", r.scope, r.id))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                (rows, next)
            }
            (None, None) => (Vec::new(), String::new()),
        };
        drop(tx);
        let mut out = Vec::new();
        for r in rows {
            let rec: PurchaseRec = r.decode()?;
            let p = effective_purchase(&rec.purchase.unwrap_or_default(), now);
            if !include_cancelled && p.cancelled_at.is_some() {
                continue;
            }
            let mut p = p;
            self.name_purchase(cx, &mut p).await;
            out.push(p);
        }
        Ok((out, next))
    }

    /// An org's purchases, by id, paged.
    pub async fn list_purchases_of_org(
        &self,
        cx: &Cx,
        org_id: &str,
        include_cancelled: bool,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<MarketplacePurchase>, String)> {
        self.purchases_page(cx, None, Some(org_id), include_cancelled, after, limit)
            .await
    }

    /// The purchasers of a listing (the publisher's view), paged.
    pub async fn list_purchases_of_listing(
        &self,
        cx: &Cx,
        listing_by: &str,
        include_cancelled: bool,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<MarketplacePurchase>, String)> {
        let listing_id = {
            let mut tx = self.db.begin(SURFACE, cx).await?;
            load_listing(&mut tx, listing_by)
                .await?
                .0
                .listing
                .map(|l| l.id)
                .unwrap_or_default()
        };
        self.purchases_page(
            cx,
            Some(("listing", Key::K1(listing_id))),
            None,
            include_cancelled,
            after,
            limit,
        )
        .await
    }

    /// The publisher org id of a listing.
    pub async fn listing_publisher(&self, cx: &Cx, by: &str) -> Result<String> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(load_listing(&mut tx, by).await?.0.publisher_org_id)
    }

    // ---------- publishers ----------

    /// A publisher's profile (an org that never asked is `UNVERIFIED`).
    pub async fn get_publisher(&self, cx: &Cx, org: &Account) -> Result<MarketplacePublisher> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let mut p = tx
            .get_msg::<PublisherRec>(&PUBLISHERS, "", &org.id)
            .await?
            .and_then(|(r, _)| r.publisher)
            .unwrap_or(MarketplacePublisher {
                org: org.name.clone(),
                verification: MarketplacePublisherVerification::Unverified as i32,
                ..Default::default()
            });
        p.org = org.name.clone();
        p.listing_count = tx
            .count(&LISTINGS, "", Some(&Key::K2(org.id.clone())))
            .await? as u32;
        Ok(p)
    }

    /// Asks for verified-publisher status. The org needs a verified domain and two-factor
    /// authentication enforced ([`DomainCheck`]); the request is then approved at once
    /// (`auto_approve`) or waits for [`Apps::review_publisher`].
    pub async fn request_publisher_verification(
        &self,
        cx: &Cx,
        org: &Account,
        domain: &str,
        support_email: &str,
    ) -> Result<MarketplacePublisher> {
        let domain = domain.trim().to_lowercase();
        if domain.is_empty() {
            return Err(ForgeError::missing("domain"));
        }
        if !support_email.is_empty() && (!support_email.contains('@') || support_email.len() > 254)
        {
            return Err(bad("bad_email", "support_email is not an email address"));
        }
        let checker: std::sync::Arc<dyn DomainCheck> = match &self.cfg.domains {
            Some(d) => d.clone(),
            None => std::sync::Arc::new(OrgDomainCheck::new(self.forge.clone())),
        };
        if !checker.is_verified(cx, &org.id, &domain).await? {
            return Err(ForgeError::invalid_state(format!(
                "{domain} is not a verified domain of {}",
                org.name
            ))
            .with_subcode("domain_not_verified"));
        }
        if !checker.two_factor_required(cx, &org.id).await? {
            return Err(ForgeError::new(
                ErrorReason::TwoFactorRequired,
                format!("{} must enforce two-factor authentication", org.name),
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let existing = tx.get_msg::<PublisherRec>(&PUBLISHERS, "", &org.id).await?;
        let mut p = existing
            .as_ref()
            .and_then(|(r, _)| r.publisher.clone())
            .unwrap_or(MarketplacePublisher {
                org: org.id.clone(),
                verification: MarketplacePublisherVerification::Unverified as i32,
                ..Default::default()
            });
        if p.verification == MarketplacePublisherVerification::Verified as i32 {
            return Err(ForgeError::invalid_state(
                "the publisher is already verified",
            ));
        }
        let before = json(&p);
        p.verified_domain = domain;
        p.support_email = support_email.to_string();
        if self.cfg.marketplace.auto_approve {
            p.verification = MarketplacePublisherVerification::Verified as i32;
            p.verified_at = tsm(now);
        } else {
            p.verification = MarketplacePublisherVerification::Pending as i32;
        }
        let rec = PublisherRec {
            publisher: Some(p.clone()),
        };
        let row = Row::new("", org.id.clone());
        match existing {
            Some((_, old)) => {
                tx.update(&PUBLISHERS, &row.version(old.version).msg(&rec))
                    .await?;
            }
            None => tx.insert(&PUBLISHERS, &row.msg(&rec)).await?,
        }
        tx.audit(
            AuditDraft::new("marketplace.publisher_verify_request", &org.id)
                .target_id(&org.id)
                .before(&before)
                .after(&json(&p)),
        )
        .await?;
        tx.commit().await?;
        self.get_publisher(cx, org).await
    }

    /// A site admin's decision on a pending verification request.
    pub async fn review_publisher(
        &self,
        cx: &Cx,
        org: &Account,
        approve: bool,
    ) -> Result<MarketplacePublisher> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let now = tx.now_ms();
        let Some((mut rec, row)) = tx.get_msg::<PublisherRec>(&PUBLISHERS, "", &org.id).await?
        else {
            return Err(ForgeError::not_found(format!("publisher {}", org.name)));
        };
        let mut p = rec.publisher.clone().unwrap_or_default();
        if p.verification != MarketplacePublisherVerification::Pending as i32 {
            return Err(ForgeError::invalid_state(
                "no verification request is pending",
            ));
        }
        let before = json(&p);
        if approve {
            p.verification = MarketplacePublisherVerification::Verified as i32;
            p.verified_at = tsm(now);
        } else {
            p.verification = MarketplacePublisherVerification::Rejected as i32;
        }
        rec.publisher = Some(p.clone());
        tx.update(&PUBLISHERS, &row.clone().msg(&rec)).await?;
        tx.audit(
            AuditDraft::new("marketplace.publisher_review", &org.id)
                .target_id(&org.id)
                .before(&before)
                .after(&json(&p)),
        )
        .await?;
        tx.commit().await?;
        self.get_publisher(cx, org).await
    }
}
