//! Inference usage records and `GetModelUsage`.
//!
//! The gateway records one row per request (or per batch of identical requests) with
//! [`Models::record_inference`]. Like billing's usage rows these are metering data: they are
//! written without an audit entry or event each, because a row per model call would swamp
//! both logs.

use std::collections::BTreeMap;

use bee_proto::v1::{Cost, ModelUsage, ModelUsageLine};

use super::records::UsageRow;
use super::{Models, SURFACE, USAGE};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Cursor, Key, Row, Scan, Tx};

const PAGE: usize = 5_000;

/// One inference (or batch) the gateway served.
#[derive(Clone, Debug, Default)]
pub struct InferenceUsage {
    /// Org billed or whose key was used.
    pub org_id: String,
    /// Principal that made the request.
    pub principal_id: String,
    /// Model id.
    pub model: String,
    /// The gateway's request id: recording the same id again is a no-op. Empty records
    /// every call.
    pub request_id: String,
    /// Requests (0 counts as 1).
    pub requests: u64,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cost in micro-USD (what the org owes or would owe the provider).
    pub usd_micros: u64,
    /// Whether the org's own provider key paid for it.
    pub byok: bool,
    /// Time, ms; 0 means now.
    pub at_ms: u64,
}

impl Models {
    /// Usage rows of an org with `from_ms <= time < to_ms` (optionally of one principal), in
    /// time order.
    pub(crate) async fn usage_rows(
        tx: &mut Tx,
        org_id: &str,
        from_ms: u64,
        to_ms: u64,
        key: Option<Key>,
    ) -> Result<Vec<(UsageRow, Row)>> {
        let cap = i64::MAX as u64;
        let mut out = Vec::new();
        let mut after: Option<Cursor> = None;
        loop {
            let mut q = Scan::scope(org_id, PAGE)
                .by_n1()
                .n1_min(from_ms.min(cap) as i64)
                .n1_max(to_ms.saturating_sub(1).min(cap) as i64)
                .after(after.take());
            if let Some(k) = &key {
                q = q.key(k.clone());
            }
            let rows = tx.scan_msgs::<UsageRow>(&USAGE, &q).await?;
            let n = rows.len();
            after = rows.last().map(|(_, r)| Cursor::after(r));
            out.extend(rows);
            if n < PAGE {
                return Ok(out);
            }
        }
    }

    /// Records inference usage; returns false when `request_id` was recorded before.
    pub async fn record_inference(&self, cx: &Cx, u: InferenceUsage) -> Result<bool> {
        if u.org_id.is_empty() {
            return Err(ForgeError::missing("org_id"));
        }
        if u.model.is_empty() {
            return Err(ForgeError::missing("model"));
        }
        if u.request_id.len() > 128 {
            return Err(ForgeError::invalid(
                "invalid_request_id",
                "request id over 128 bytes",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let id = if u.request_id.is_empty() {
            tx.new_id()?
        } else {
            u.request_id.clone()
        };
        if tx.get(&USAGE, &u.org_id, &id).await?.is_some() {
            return Ok(false);
        }
        let at = if u.at_ms == 0 { tx.now_ms() } else { u.at_ms };
        let row = UsageRow {
            model: u.model.clone(),
            principal_id: u.principal_id.clone(),
            requests: u.requests.max(1),
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            usd_micros: u.usd_micros,
            byok: u.byok,
            at_ms: at,
        };
        tx.insert(
            &USAGE,
            &Row::new(u.org_id.clone(), id)
                .k1(u.model)
                .k2(u.principal_id)
                .n1(at as i64)
                .msg(&row),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Model usage of an org over a window (`start` inclusive, `end` exclusive; open ends are
    /// unbounded), per model, optionally per principal, and for one principal when
    /// `principal_id` is set, with the org's model budget.
    pub async fn usage(
        &self,
        cx: &Cx,
        org_id: &str,
        principal_id: &str,
        start_ms: Option<u64>,
        end_ms: Option<u64>,
        group_by_principal: bool,
    ) -> Result<ModelUsage> {
        if let (Some(s), Some(e)) = (start_ms, end_ms)
            && s >= e
        {
            return Err(ForgeError::invalid(
                "invalid_window",
                "window_start must be before window_end",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let key = (!principal_id.is_empty()).then(|| Key::K2(principal_id.to_string()));
        let rows = Self::usage_rows(
            &mut tx,
            org_id,
            start_ms.unwrap_or(0),
            end_ms.unwrap_or(u64::MAX),
            key,
        )
        .await?;
        let budget = tx
            .get_msg::<super::records::PolicyRow>(&super::POLICY, org_id, "policy")
            .await?
            .map(|(p, _)| p.budget_usd_micros)
            .unwrap_or(0);
        let mut lines: BTreeMap<(String, String, bool), ModelUsageLine> = BTreeMap::new();
        let mut total = Cost::default();
        for (u, _) in rows {
            let principal = if group_by_principal || !principal_id.is_empty() {
                u.principal_id.clone()
            } else {
                String::new()
            };
            let l = lines
                .entry((u.model.clone(), principal.clone(), u.byok))
                .or_insert_with(|| ModelUsageLine {
                    model: u.model.clone(),
                    principal_id: principal,
                    requests: 0,
                    cost: Some(Cost::default()),
                    byok: u.byok,
                });
            l.requests += u.requests;
            if let Some(c) = l.cost.as_mut() {
                c.input_tokens += u.input_tokens;
                c.output_tokens += u.output_tokens;
                c.usd_micros += u.usd_micros;
            }
            total.input_tokens += u.input_tokens;
            total.output_tokens += u.output_tokens;
            total.usd_micros += u.usd_micros;
        }
        Ok(ModelUsage {
            org: org_id.to_string(),
            window_start: start_ms.map(ts),
            window_end: end_ms.map(ts),
            lines: lines.into_values().collect(),
            total: Some(total),
            budget_usd_micros: budget,
        })
    }
}
