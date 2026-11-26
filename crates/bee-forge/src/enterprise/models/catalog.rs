//! The model catalog, org model policy, provider keys (BYOK) and inference admission.

use bee_proto::v1::error_detail::Payload;
use bee_proto::v1::{
    BillingProduct, CreateModelProviderKeyRequest, ErrorReason, LimitExceeded, ModelInfo,
    ModelProviderKey,
};

use super::records::PolicyRow;
use super::{CATALOG, Change, KEYS, Models, POLICY, SURFACE, record};
use crate::cx::Cx;
use crate::enterprise::billing::time::{month_of, month_start_ms};
use crate::enterprise::common::{
    MAX_LIST, SCAN_CAP, get_or_404, json, principal_of, scan_all, scan_page, valid_slug,
};
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Key, Row, Scan, Tx};

/// Capabilities a catalog model may declare.
pub const CAPABILITIES: &[&str] = &["chat", "tools", "vision", "embeddings", "reasoning"];
/// Most provider keys one org registers.
pub const MAX_KEYS_PER_ORG: usize = 100;

/// An org's model policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct OrgModelPolicy {
    /// Model ids the org allows; `None` allows every model of the catalog.
    pub allowed_models: Option<Vec<String>>,
    /// Monthly budget (micro-USD) for gateway-paid inference, 0 for none. Inference through
    /// the org's own provider keys is not counted.
    pub budget_usd_micros: u64,
}

/// What an inference request is expected to consume, for admission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InferenceEstimate {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Most output tokens the request allows.
    pub max_output_tokens: u64,
}

/// How an admitted inference request is routed.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    /// The model.
    pub model: ModelInfo,
    /// Whether the org's own provider key is used (and pays).
    pub byok: bool,
    /// The provider key used, when `byok`.
    pub provider_key_id: String,
    /// Estimated cost for gateway-paid inference, micro-USD (0 when `byok`).
    pub estimated_usd_micros: u64,
}

fn key_covers(k: &ModelProviderKey, m: &ModelInfo) -> bool {
    k.provider == m.provider && (k.models.is_empty() || k.models.contains(&m.id))
}

fn secret_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
}

impl Models {
    /// Adds or replaces a catalog model (administrator operation; the id is
    /// `<provider>/<name>`). `enabled_for_org` is computed per org and never stored.
    pub async fn upsert_model(&self, cx: &Cx, mut m: ModelInfo) -> Result<ModelInfo> {
        if m.id.is_empty() || m.id.len() > 200 {
            return Err(ForgeError::invalid(
                "invalid_model",
                "model id is 1-200 characters",
            ));
        }
        if m.provider.is_empty() || !m.id.starts_with(&format!("{}/", m.provider)) {
            return Err(ForgeError::invalid(
                "invalid_model",
                "model id must be `<provider>/<name>` with the model's provider",
            ));
        }
        if let Some(c) = m
            .capabilities
            .iter()
            .find(|c| !CAPABILITIES.contains(&c.as_str()))
        {
            return Err(ForgeError::invalid(
                "unknown_capability",
                format!("unknown capability {c:?}"),
            ));
        }
        if m.context_window_tokens == 0 || (m.max_output_tokens > m.context_window_tokens) {
            return Err(ForgeError::invalid(
                "invalid_model",
                "context window must be positive and at least the output limit",
            ));
        }
        m.capabilities.sort();
        m.capabilities.dedup();
        m.enabled_for_org = false;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let before = tx
            .get_msg::<ModelInfo>(&CATALOG, "", &m.id)
            .await?
            .map(|(b, _)| json(&b));
        let row = Row::new("", m.id.clone()).k1(m.provider.clone()).msg(&m);
        tx.put(&CATALOG, &row).await?;
        record(
            &mut tx,
            Change {
                kind: "model.catalog_updated",
                action: "model.catalog.upsert",
                org_id: "",
                repo_id: "",
                target: &m.id,
                before: before.as_deref(),
                after: &json(&m),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(m)
    }

    /// Removes a catalog model.
    pub async fn remove_model(&self, cx: &Cx, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (m, _) = get_or_404::<ModelInfo>(&mut tx, &CATALOG, "", id, "model").await?;
        tx.delete(&CATALOG, "", id).await?;
        record(
            &mut tx,
            Change {
                kind: "model.catalog_updated",
                action: "model.catalog.remove",
                org_id: "",
                repo_id: "",
                target: id,
                before: Some(&json(&m)),
                after: "{}",
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn policy_row(tx: &mut Tx, org_id: &str) -> Result<PolicyRow> {
        Ok(tx
            .get_msg::<PolicyRow>(&POLICY, org_id, "policy")
            .await?
            .map(|(p, _)| p)
            .unwrap_or_default())
    }

    fn policy_of(row: PolicyRow) -> OrgModelPolicy {
        OrgModelPolicy {
            allowed_models: row.restrict.then_some(row.allowed),
            budget_usd_micros: row.budget_usd_micros,
        }
    }

    /// An org's model policy (everything allowed, no budget, unless set).
    pub async fn org_policy(&self, cx: &Cx, org_id: &str) -> Result<OrgModelPolicy> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        Ok(Self::policy_of(Self::policy_row(&mut tx, org_id).await?))
    }

    /// Sets an org's model policy: an optional allow list of catalog models and the monthly
    /// budget for gateway-paid inference. Called by whatever manages org settings.
    pub async fn set_org_policy(
        &self,
        cx: &Cx,
        org_id: &str,
        p: OrgModelPolicy,
    ) -> Result<OrgModelPolicy> {
        if org_id.is_empty() {
            return Err(ForgeError::missing("org"));
        }
        let mut allowed = p.allowed_models.clone().unwrap_or_default();
        allowed.sort();
        allowed.dedup();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for id in &allowed {
            if tx.get(&CATALOG, "", id).await?.is_none() {
                return Err(ForgeError::not_found(format!("model {id}")));
            }
        }
        let before = json(&Self::policy_of(Self::policy_row(&mut tx, org_id).await?));
        let row = PolicyRow {
            restrict: p.allowed_models.is_some(),
            allowed,
            budget_usd_micros: p.budget_usd_micros,
        };
        tx.put(&POLICY, &Row::new(org_id, "policy").msg(&row))
            .await?;
        let out = Self::policy_of(row);
        record(
            &mut tx,
            Change {
                kind: "model.policy_updated",
                action: "model.policy.update",
                org_id,
                repo_id: "",
                target: org_id,
                before: Some(&before),
                after: &json(&out),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Whether the org may use `m`: the allow list admits it and, for BYOK-only models, the
    /// org has a provider key covering it.
    fn enabled(m: &ModelInfo, policy: &PolicyRow, keys: &[ModelProviderKey]) -> bool {
        let allowed = !policy.restrict || policy.allowed.contains(&m.id);
        let byok_ok = !m.byok_only || keys.iter().any(|k| key_covers(k, m));
        allowed && byok_ok
    }

    async fn org_context(
        tx: &mut Tx,
        org_id: &str,
    ) -> Result<Option<(PolicyRow, Vec<ModelProviderKey>)>> {
        if org_id.is_empty() {
            return Ok(None);
        }
        let policy = Self::policy_row(tx, org_id).await?;
        let keys = scan_all::<ModelProviderKey>(tx, &KEYS, org_id, None)
            .await?
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        Ok(Some((policy, keys)))
    }

    fn mark(m: &mut ModelInfo, ctx: &Option<(PolicyRow, Vec<ModelProviderKey>)>) {
        m.enabled_for_org = match ctx {
            Some((p, k)) => Self::enabled(m, p, k),
            None => !m.byok_only,
        };
    }

    /// Catalog models ordered by id, filtered by provider and capability, with org policy
    /// applied when `org_id` is not empty; returns the page and the next token.
    pub async fn list_models(
        &self,
        cx: &Cx,
        org_id: &str,
        provider: &str,
        capability: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ModelInfo>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let key = (!provider.is_empty()).then(|| Key::K1(provider.to_string()));
        let ctx = Self::org_context(&mut tx, org_id).await?;
        let limit = limit.clamp(1, MAX_LIST);
        let mut out: Vec<ModelInfo> = scan_all::<ModelInfo>(&mut tx, &CATALOG, "", key)
            .await?
            .into_iter()
            .map(|(m, _)| m)
            .filter(|m| capability.is_empty() || m.capabilities.iter().any(|c| c == capability))
            .filter(|m| after.is_empty() || m.id.as_str() > after)
            .collect();
        let next = if out.len() > limit {
            out.truncate(limit);
            out.last().map(|m| m.id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        for m in &mut out {
            Self::mark(m, &ctx);
        }
        Ok((out, next))
    }

    /// One catalog model with org policy applied when `org_id` is not empty.
    pub async fn get_model(&self, cx: &Cx, org_id: &str, id: &str) -> Result<ModelInfo> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let ctx = Self::org_context(&mut tx, org_id).await?;
        let (mut m, _) = get_or_404::<ModelInfo>(&mut tx, &CATALOG, "", id, "model").await?;
        Self::mark(&mut m, &ctx);
        Ok(m)
    }

    /// Registers an org provider key (BYOK). The secret itself lives in the secret service;
    /// only its name is stored. The first key of a provider becomes its default, and setting
    /// `is_default` replaces the previous default.
    pub async fn create_provider_key(
        &self,
        cx: &Cx,
        org_id: &str,
        req: CreateModelProviderKeyRequest,
    ) -> Result<ModelProviderKey> {
        let name = req.name.trim().to_string();
        if name.is_empty() || name.len() > 100 {
            return Err(ForgeError::invalid(
                "invalid_name",
                "key name is 1-100 characters",
            ));
        }
        if !valid_slug(&req.provider) {
            return Err(ForgeError::invalid(
                "invalid_provider",
                "provider is a lower-case slug",
            ));
        }
        if !secret_name_ok(&req.secret_name) {
            return Err(ForgeError::invalid(
                "invalid_secret_name",
                "secret_name is 1-200 characters of letters, digits and `_-./`",
            ));
        }
        if !req.endpoint.is_empty()
            && !(req.endpoint.len() <= 2000
                && (req.endpoint.starts_with("https://") || req.endpoint.starts_with("http://")))
        {
            return Err(ForgeError::invalid(
                "bad_url",
                "endpoint must be an http(s) URL",
            ));
        }
        let created_by = principal_of(&self.forge, &cx.actor_id).await;
        let mut models: Vec<String> = req.models.clone();
        models.sort();
        models.dedup();
        let mut tx = self.db.begin(SURFACE, cx).await?;
        for id in &models {
            let (m, _) = get_or_404::<ModelInfo>(&mut tx, &CATALOG, "", id, "model").await?;
            if m.provider != req.provider {
                return Err(ForgeError::invalid(
                    "unknown_model",
                    format!("model {id} is not a {} model", req.provider),
                ));
            }
        }
        let keys = scan_all::<ModelProviderKey>(&mut tx, &KEYS, org_id, None).await?;
        if keys.len() >= MAX_KEYS_PER_ORG {
            return Err(ForgeError::too_large(
                "provider_keys",
                MAX_KEYS_PER_ORG as u64,
                keys.len() as u64 + 1,
            ));
        }
        if keys.iter().any(|(k, _)| k.name == name) {
            return Err(ForgeError::already_exists(format!("provider key {name}")));
        }
        let first = !keys.iter().any(|(k, _)| k.provider == req.provider);
        let is_default = req.is_default || first;
        if is_default {
            for (mut k, row) in keys {
                if k.provider == req.provider && k.is_default {
                    k.is_default = false;
                    tx.update(&KEYS, &row.msg(&k)).await?;
                }
            }
        }
        let k = ModelProviderKey {
            id: tx.new_id()?,
            org: org_id.to_string(),
            provider: req.provider,
            name,
            secret_name: req.secret_name,
            endpoint: req.endpoint,
            models,
            is_default,
            created_by: Some(created_by),
            created_at: Some(ts(tx.now_ms())),
        };
        tx.insert(
            &KEYS,
            &Row::new(org_id, k.id.clone())
                .k1(k.provider.clone())
                .k2(k.name.clone())
                .msg(&k),
        )
        .await?;
        record(
            &mut tx,
            Change {
                kind: "model.key_created",
                action: "model.key.create",
                org_id,
                repo_id: "",
                target: &k.id,
                before: None,
                after: &json(&k),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(k)
    }

    /// An org's provider keys ordered by id (never secret values).
    pub async fn list_provider_keys(
        &self,
        cx: &Cx,
        org_id: &str,
        after: &str,
        limit: usize,
    ) -> Result<(Vec<ModelProviderKey>, String)> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (rows, next) =
            scan_page::<ModelProviderKey>(&mut tx, &KEYS, org_id, None, after, limit).await?;
        Ok((rows.into_iter().map(|(k, _)| k).collect(), next))
    }

    /// Removes a provider key. When it was its provider's default, the oldest remaining key
    /// of that provider becomes the default.
    pub async fn delete_provider_key(&self, cx: &Cx, org_id: &str, id: &str) -> Result<()> {
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (k, _) =
            get_or_404::<ModelProviderKey>(&mut tx, &KEYS, org_id, id, "provider key").await?;
        tx.delete(&KEYS, org_id, id).await?;
        if k.is_default {
            let rest = tx
                .scan_msgs::<ModelProviderKey>(
                    &KEYS,
                    &Scan::scope(org_id, SCAN_CAP).key(Key::K1(k.provider.clone())),
                )
                .await?;
            if let Some((mut next, row)) = rest.into_iter().next() {
                next.is_default = true;
                tx.update(&KEYS, &row.msg(&next)).await?;
            }
        }
        record(
            &mut tx,
            Change {
                kind: "model.key_deleted",
                action: "model.key.delete",
                org_id,
                repo_id: "",
                target: id,
                before: Some(&json(&k)),
                after: "{}",
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Admits one inference request of `org_id` for `model_id`, or refuses it. In order:
    ///
    /// 1. the model must be in the catalog (`NOT_FOUND`) and allowed by the org's allow list
    ///    (`POLICY_DENIED` / `model_not_allowed`);
    /// 2. routing: a named `provider_key_id` must belong to the org and cover the model;
    ///    otherwise the provider's default key if it covers the model; otherwise the
    ///    gateway's own key, which a BYOK-only model refuses (`INVALID_STATE` /
    ///    `byok_key_required`);
    /// 3. gateway-paid requests then must fit the org's model budget for the month and the
    ///    billing [`SpendGate`] for `AGENT_TOKENS` (`QUOTA_EXCEEDED`).
    pub async fn admit_inference(
        &self,
        cx: &Cx,
        org_id: &str,
        model_id: &str,
        provider_key_id: &str,
        est: InferenceEstimate,
    ) -> Result<Route> {
        if org_id.is_empty() {
            return Err(ForgeError::missing("org"));
        }
        if model_id.is_empty() {
            return Err(ForgeError::missing("model"));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let (model, _) = get_or_404::<ModelInfo>(&mut tx, &CATALOG, "", model_id, "model").await?;
        let policy = Self::policy_row(&mut tx, org_id).await?;
        if policy.restrict && !policy.allowed.iter().any(|a| a == model_id) {
            return Err(ForgeError::policy_denied(
                "model_not_allowed",
                vec![model_id.to_string()],
                format!("org policy does not allow model {model_id}"),
            ));
        }
        let keys: Vec<ModelProviderKey> =
            scan_all::<ModelProviderKey>(&mut tx, &KEYS, org_id, None)
                .await?
                .into_iter()
                .map(|(k, _)| k)
                .collect();
        let key = if provider_key_id.is_empty() {
            keys.iter()
                .find(|k| k.is_default && key_covers(k, &model))
                .or_else(|| keys.iter().find(|k| key_covers(k, &model)))
                .cloned()
        } else {
            let k = keys
                .iter()
                .find(|k| k.id == provider_key_id)
                .cloned()
                .ok_or_else(|| ForgeError::not_found(format!("provider key {provider_key_id}")))?;
            if !key_covers(&k, &model) {
                return Err(ForgeError::invalid(
                    "key_does_not_cover_model",
                    format!("key {provider_key_id} does not serve model {model_id}"),
                ));
            }
            Some(k)
        };
        if key.is_none() && model.byok_only {
            return Err(ForgeError::invalid_state(format!(
                "model {model_id} needs a provider key of the org"
            ))
            .with_subcode("byok_key_required"));
        }
        if let Some(k) = key {
            return Ok(Route {
                model,
                byok: true,
                provider_key_id: k.id,
                estimated_usd_micros: 0,
            });
        }
        let price = model.pricing.unwrap_or_default();
        let max_output = if est.max_output_tokens == 0 {
            model.max_output_tokens
        } else {
            est.max_output_tokens
        };
        let estimate = (u128::from(est.input_tokens) * u128::from(price.input_usd_micros_per_mtok)
            + u128::from(max_output) * u128::from(price.output_usd_micros_per_mtok))
            / 1_000_000;
        let estimate = estimate as u64;
        if policy.budget_usd_micros > 0 {
            let from = month_start_ms(month_of(tx.now_ms()));
            let spent: u64 = Self::usage_rows(&mut tx, org_id, from, u64::MAX, None)
                .await?
                .iter()
                .filter(|(u, _)| !u.byok)
                .map(|(u, _)| u.usd_micros)
                .sum();
            if spent >= policy.budget_usd_micros
                || spent.saturating_add(estimate) > policy.budget_usd_micros
            {
                return Err(ForgeError::new(
                    ErrorReason::QuotaExceeded,
                    format!(
                        "org model budget of {} micro-USD: {spent} spent, about {estimate} more requested",
                        policy.budget_usd_micros
                    ),
                )
                .with_subcode("model_budget_exhausted")
                .with_payload(Payload::LimitExceeded(LimitExceeded {
                    limit: "org model budget".into(),
                    maximum: policy.budget_usd_micros,
                    actual: spent.saturating_add(estimate),
                })));
            }
        }
        drop(tx);
        if let Some(gate) = &self.gate {
            gate.check_spend(cx, org_id, "", BillingProduct::AgentTokens, estimate)
                .await?;
        }
        Ok(Route {
            model,
            byok: false,
            provider_key_id: String::new(),
            estimated_usd_micros: estimate,
        })
    }
}
