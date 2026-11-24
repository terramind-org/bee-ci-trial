//! Invoices: generation at the end of a billing cycle, the status lifecycle and signed PDF
//! URLs.
//!
//! # Cycle and invoice
//!
//! An account's term is one UTC month (or twelve, on a yearly cycle) starting on the first
//! of a month. [`Billing::close_cycle`] invoices every term that has ended by `now` (more
//! than one when it was not called for a while): the purchased seats at the plan's seat
//! price for the cycle, plus the net metered usage of the term from the ledger, one line per
//! product and SKU in product then SKU order. Lines of zero net amount are left out and a
//! term that totals zero produces no invoice. Pending plan changes then take effect and the
//! next term begins. Nothing is prorated.
//!
//! The invoice number is `INV-<yyyymm of the period start>-<seq>` where `seq` counts the
//! invoices of that period month across all accounts.
//!
//! # Status
//!
//! ```text
//! DRAFT -> OPEN -> PAID
//!   |        |  \-> UNCOLLECTIBLE -> PAID
//!   \--------+-> VOID
//! ```

use std::collections::BTreeMap;

use bee_proto::v1::{BillingCycle, BillingProduct, Invoice, InvoiceLineItem, InvoiceStatus};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::plans::term_end_ms;
use super::time::{month_of, yyyymm};
use super::{Billing, COUNTERS, Change, INVOICES, SURFACE, event_org, record_change};
use crate::cx::Cx;
use crate::enterprise::common::{ResolvedScope, ct_eq, get_or_404, json};
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts};
use crate::store::{Cursor, Key, Row, Scan};

const DAY_MS: u64 = 86_400_000;

/// A signed, short-lived invoice PDF link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvoiceUrl {
    /// `<base_url>/billing/invoices/<id>.pdf?exp=<unix seconds>&sig=<hex>`.
    pub url: String,
    /// Expiry, ms.
    pub expires_at_ms: u64,
}

fn status_of(i: &Invoice) -> InvoiceStatus {
    InvoiceStatus::try_from(i.status).unwrap_or(InvoiceStatus::Unspecified)
}

#[derive(Default)]
struct UsageLine {
    quantity_micro: u128,
    covered_micro: u128,
    net: u64,
    unit: String,
    price: u64,
}

impl Billing {
    /// Invoices every ended term of the account as `OPEN` invoices (see the module docs);
    /// returns the invoices created, oldest first. Idempotent: a term is invoiced once.
    pub async fn close_cycle(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        now_ms: u64,
    ) -> Result<Vec<Invoice>> {
        self.close_with(cx, scope, now_ms, InvoiceStatus::Open)
            .await
    }

    /// As [`Billing::close_cycle`] but the invoices are created as `DRAFT` until
    /// [`Billing::finalize_invoice`].
    pub async fn close_cycle_draft(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        now_ms: u64,
    ) -> Result<Vec<Invoice>> {
        self.close_with(cx, scope, now_ms, InvoiceStatus::Draft)
            .await
    }

    async fn close_with(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        now_ms: u64,
        status: InvoiceStatus,
    ) -> Result<Vec<Invoice>> {
        self.require_enabled()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut acct, row) = self.load_account(&mut tx, scope).await?;
        let mut row = row;
        let mut out = Vec::new();
        loop {
            let (start, end) = (
                acct.term_start_ms,
                term_end_ms(acct.term_start_ms, acct.cycle),
            );
            if end > now_ms {
                break;
            }
            let ctx = self.ledger_ctx_with(&mut tx, scope, &acct.plan_id).await?;
            let mut lines: Vec<InvoiceLineItem> = Vec::new();
            if let Some(plan) = self.plan(&acct.plan_id)
                && acct.seats_purchased > 0
            {
                let yearly = acct.cycle == BillingCycle::Yearly as i32;
                let price = if yearly {
                    plan.seat_yearly_price_micro_usd
                } else {
                    plan.seat_monthly_price_micro_usd
                };
                if price > 0 {
                    lines.push(InvoiceLineItem {
                        description: format!(
                            "{} plan: {} seats ({})",
                            plan.name,
                            acct.seats_purchased,
                            if yearly { "yearly" } else { "monthly" }
                        ),
                        product: BillingProduct::Seats as i32,
                        sku: if yearly {
                            "seat_yearly"
                        } else {
                            "seat_monthly"
                        }
                        .into(),
                        quantity: f64::from(acct.seats_purchased),
                        unit: "seat".into(),
                        unit_price_micro_usd: price,
                        amount_micro_usd: price.saturating_mul(u64::from(acct.seats_purchased)),
                    });
                }
            }
            let mut usage: BTreeMap<(i32, String), UsageLine> = BTreeMap::new();
            for mi in month_of(start)..=month_of(end - 1) {
                for p in self.month_ledger(&mut tx, scope, &ctx, mi).await? {
                    let l = usage.entry((p.product, p.sku.clone())).or_default();
                    l.quantity_micro += u128::from(p.quantity_micro);
                    l.covered_micro += u128::from(p.discount_quantity_micro);
                    l.net += p.net;
                    l.unit = p.unit.clone();
                    l.price = p.unit_price;
                }
            }
            for ((product, sku), l) in usage {
                if l.net == 0 {
                    continue;
                }
                let billable = l.quantity_micro.saturating_sub(l.covered_micro);
                lines.push(InvoiceLineItem {
                    description: if l.covered_micro > 0 {
                        format!(
                            "{sku} ({} {} included)",
                            l.covered_micro as f64 / 1_000_000.0,
                            l.unit
                        )
                    } else {
                        sku.clone()
                    },
                    product,
                    sku,
                    quantity: billable as f64 / 1_000_000.0,
                    unit: l.unit,
                    unit_price_micro_usd: l.price,
                    amount_micro_usd: l.net,
                });
            }
            let subtotal: u64 = lines.iter().map(|l| l.amount_micro_usd).sum();
            if subtotal > 0 {
                let tax = ((u128::from(subtotal) * u128::from(self.cfg.tax_rate_bps) + 5_000)
                    / 10_000) as u64;
                let seq = tx
                    .add_n1(
                        &COUNTERS,
                        "",
                        &format!("inv:{}", yyyymm(month_of(start))),
                        1,
                    )
                    .await?;
                let open = status == InvoiceStatus::Open;
                let inv = Invoice {
                    id: tx.new_id()?,
                    number: format!("INV-{}-{seq:04}", yyyymm(month_of(start))),
                    scope: None,
                    status: status as i32,
                    period_start_at: Some(ts(start)),
                    period_end_at: Some(ts(end)),
                    issued_at: open.then(|| ts(now_ms)),
                    due_at: open
                        .then(|| ts(now_ms + u64::from(self.cfg.invoice_due_days) * DAY_MS)),
                    paid_at: None,
                    subtotal_micro_usd: subtotal,
                    tax_micro_usd: tax,
                    total_micro_usd: subtotal + tax,
                    amount_paid_micro_usd: 0,
                    line_items: lines,
                };
                tx.insert(
                    &INVOICES,
                    &Row::new(scope.key(), inv.id.clone())
                        .k1(status.as_str_name())
                        .k2(format!("{start:020}"))
                        .n1(start as i64)
                        .msg(&inv),
                )
                .await?;
                let view = Self::invoice_view(scope, inv, true);
                record_change(
                    &mut tx,
                    scope,
                    Change {
                        kind: "billing.invoice_created",
                        action: "billing.invoice.create",
                        target: &view.id,
                        before: None,
                        resource: &json(&view),
                        event_org: &event_org(scope),
                    },
                )
                .await?;
                out.push(view);
            }
            if !acct.pending_plan_id.is_empty() {
                acct.plan_id = std::mem::take(&mut acct.pending_plan_id);
                acct.seats_purchased = acct.pending_seats;
            } else if acct.pending_seats > 0 {
                acct.seats_purchased = acct.pending_seats;
            }
            acct.pending_seats = 0;
            acct.term_start_ms = end;
            acct.updated_ms = now_ms;
            // The account row must exist (and carry the new version) before the next term.
            Self::save_account(&mut tx, scope, &acct, row.take()).await?;
            row = tx.get(&super::ACCOUNTS, &scope.key(), "plan").await?;
        }
        tx.commit().await?;
        Ok(out)
    }

    fn invoice_view(scope: &ResolvedScope, mut i: Invoice, lines: bool) -> Invoice {
        i.scope = Some(scope.to_proto());
        if !lines {
            i.line_items.clear();
        }
        i
    }

    /// Invoices newest first (by period, then id), optionally of one status; returns the page
    /// (without line items) and the next token.
    pub async fn list_invoices(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        status: InvoiceStatus,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<Invoice>, String)> {
        self.require_enabled()?;
        let limit = limit.clamp(1, crate::enterprise::common::MAX_LIST);
        let cursor = if after.is_empty() {
            None
        } else {
            let (n, id) = after
                .split_once(':')
                .ok_or_else(|| ForgeError::invalid("bad_page_token", "malformed page token"))?;
            Some(Cursor {
                n1: n
                    .parse()
                    .map_err(|_| ForgeError::invalid("bad_page_token", "malformed page token"))?,
                scope: scope.key(),
                id: id.to_string(),
            })
        };
        let mut q = Scan::scope(scope.key(), limit + 1)
            .by_n1()
            .desc()
            .after(cursor);
        if status != InvoiceStatus::Unspecified {
            q = q.key(Key::K1(status.as_str_name().to_string()));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let rows = tx.scan_msgs::<Invoice>(&INVOICES, &q).await?;
        let more = rows.len() > limit;
        let rows: Vec<(Invoice, Row)> = rows.into_iter().take(limit).collect();
        let next = if more {
            rows.last()
                .map(|(_, r)| format!("{}:{}", r.n1.unwrap_or(0), r.id))
                .unwrap_or_default()
        } else {
            String::new()
        };
        Ok((
            rows.into_iter()
                .map(|(i, _)| Self::invoice_view(scope, i, false))
                .collect(),
            next,
        ))
    }

    /// One invoice with its line items and a signed PDF URL.
    pub async fn get_invoice(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
    ) -> Result<(Invoice, InvoiceUrl)> {
        self.require_enabled()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (i, _) = get_or_404::<Invoice>(&mut tx, &INVOICES, &scope.key(), id, "invoice").await?;
        let url = self.signed_invoice_url(&i.id, self.db.now_ms())?;
        Ok((Self::invoice_view(scope, i, true), url))
    }

    fn mac(&self, id: &str, exp_secs: u64) -> Result<String> {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.cfg.invoice_url_key)
            .map_err(|_| ForgeError::internal("invoice url key"))?;
        m.update(format!("invoice:{id}:{exp_secs}").as_bytes());
        Ok(hex(&m.finalize().into_bytes()))
    }

    /// A signed PDF URL for `invoice_id`, valid for the configured lifetime from `now_ms`.
    pub fn signed_invoice_url(&self, invoice_id: &str, now_ms: u64) -> Result<InvoiceUrl> {
        let exp = now_ms / 1000 + self.cfg.invoice_url_ttl_secs;
        let sig = self.mac(invoice_id, exp)?;
        Ok(InvoiceUrl {
            url: format!(
                "{}/billing/invoices/{invoice_id}.pdf?exp={exp}&sig={sig}",
                self.cfg.invoice_base_url.trim_end_matches('/')
            ),
            expires_at_ms: exp * 1000,
        })
    }

    /// Verifies a URL made by [`Billing::signed_invoice_url`] at `now_ms` and returns the
    /// invoice id it grants. `PERMISSION_DENIED` with subcode `bad_signature` or
    /// `url_expired`; `INVALID_ARGUMENT` / `bad_invoice_url` when malformed.
    pub fn verify_invoice_url(&self, url: &str, now_ms: u64) -> Result<String> {
        let bad = || ForgeError::invalid("bad_invoice_url", "not an invoice URL");
        let (path, query) = url.split_once('?').ok_or_else(bad)?;
        let id = path
            .rsplit_once("/billing/invoices/")
            .and_then(|(_, rest)| rest.strip_suffix(".pdf"))
            .filter(|id| !id.is_empty() && !id.contains('/'))
            .ok_or_else(bad)?;
        let mut exp = None;
        let mut sig = None;
        for kv in query.split('&') {
            match kv.split_once('=') {
                Some(("exp", v)) => exp = v.parse::<u64>().ok(),
                Some(("sig", v)) => sig = Some(v),
                _ => {}
            }
        }
        let (exp, sig) = exp.zip(sig).ok_or_else(bad)?;
        if !ct_eq(&self.mac(id, exp)?, sig) {
            return Err(
                ForgeError::permission_denied("invoice URL signature is wrong")
                    .with_subcode("bad_signature"),
            );
        }
        if now_ms / 1000 >= exp {
            return Err(ForgeError::permission_denied("invoice URL has expired")
                .with_subcode("url_expired"));
        }
        Ok(id.to_string())
    }

    async fn transition(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        to: InvoiceStatus,
        now_ms: u64,
    ) -> Result<Invoice> {
        self.require_enabled()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (mut inv, row) =
            get_or_404::<Invoice>(&mut tx, &INVOICES, &scope.key(), id, "invoice").await?;
        let from = status_of(&inv);
        if from == to {
            return Ok(Self::invoice_view(scope, inv, true));
        }
        use InvoiceStatus as S;
        let allowed = matches!(
            (from, to),
            (S::Draft, S::Open)
                | (S::Open | S::Uncollectible, S::Paid)
                | (S::Draft | S::Open, S::Void)
                | (S::Open, S::Uncollectible)
        );
        if !allowed {
            return Err(ForgeError::invalid_state(format!(
                "invoice {} is {} and cannot become {}",
                inv.number,
                from.as_str_name(),
                to.as_str_name()
            )));
        }
        let before = json(&Self::invoice_view(scope, inv.clone(), false));
        inv.status = to as i32;
        match to {
            S::Open => {
                inv.issued_at = Some(ts(now_ms));
                inv.due_at = Some(ts(now_ms + u64::from(self.cfg.invoice_due_days) * DAY_MS));
            }
            S::Paid => {
                inv.paid_at = Some(ts(now_ms));
                inv.amount_paid_micro_usd = inv.total_micro_usd;
            }
            _ => {}
        }
        tx.update(&INVOICES, &row.k1(to.as_str_name()).msg(&inv))
            .await?;
        let view = Self::invoice_view(scope, inv, true);
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.invoice_updated",
                action: &format!(
                    "billing.invoice.{}",
                    to.as_str_name()
                        .trim_start_matches("INVOICE_STATUS_")
                        .to_lowercase()
                ),
                target: id,
                before: Some(&before),
                resource: &json(&Self::invoice_view(scope, view.clone(), false)),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(view)
    }

    /// `DRAFT` to `OPEN`: issues the invoice now.
    pub async fn finalize_invoice(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        now_ms: u64,
    ) -> Result<Invoice> {
        self.transition(cx, scope, id, InvoiceStatus::Open, now_ms)
            .await
    }

    /// `OPEN` or `UNCOLLECTIBLE` to `PAID`, with the full total paid. Called by the payment
    /// integration.
    pub async fn mark_invoice_paid(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        now_ms: u64,
    ) -> Result<Invoice> {
        self.transition(cx, scope, id, InvoiceStatus::Paid, now_ms)
            .await
    }

    /// `DRAFT` or `OPEN` to `VOID`.
    pub async fn void_invoice(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        now_ms: u64,
    ) -> Result<Invoice> {
        self.transition(cx, scope, id, InvoiceStatus::Void, now_ms)
            .await
    }

    /// `OPEN` to `UNCOLLECTIBLE`.
    pub async fn mark_invoice_uncollectible(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        id: &str,
        now_ms: u64,
    ) -> Result<Invoice> {
        self.transition(cx, scope, id, InvoiceStatus::Uncollectible, now_ms)
            .await
    }
}
