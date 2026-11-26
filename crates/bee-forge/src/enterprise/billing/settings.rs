//! Payment method summary and billing email settings.

use bee_proto::v1::{
    BillingEmailSettings, BillingPaymentMethod, BillingPaymentMethodKind,
    UpdateBillingEmailSettingsRequest,
};

use super::{ACCOUNTS, Billing, Change, SURFACE, event_org, record_change};
use crate::cx::Cx;
use crate::enterprise::common::{ResolvedScope, json};
use crate::error::{ForgeError, Result};
use crate::ids::ts;
use crate::store::{Row, Tx};

/// Most additional billing email addresses.
pub const MAX_EMAILS: usize = 20;

/// The payment method summary the billing portal integration reports. No card number or bank
/// details ever reach the forge.
#[derive(Clone, Debug, Default)]
pub struct PaymentMethodInput {
    /// Kind of payment method on file.
    pub kind: BillingPaymentMethodKind,
    /// Card brand (cards).
    pub card_brand: String,
    /// Last four digits (cards).
    pub card_last4: String,
    /// Expiry month 1-12 (cards).
    pub card_exp_month: u32,
    /// Expiry year (cards).
    pub card_exp_year: u32,
    /// Name on the account.
    pub billing_name: String,
    /// ISO 3166-1 alpha-2 country.
    pub country: String,
}

fn valid_email(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else {
        return false;
    };
    e.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !e.chars().any(char::is_whitespace)
}

impl Billing {
    /// The payment method on file (`NONE` when the portal never reported one).
    pub async fn get_payment_method(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
    ) -> Result<BillingPaymentMethod> {
        self.require_enabled()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let stored = tx
            .get_msg::<BillingPaymentMethod>(&ACCOUNTS, &scope.key(), "payment")
            .await?
            .map(|(m, _)| m);
        let mut m = stored.unwrap_or(BillingPaymentMethod {
            kind: BillingPaymentMethodKind::None as i32,
            ..Default::default()
        });
        m.scope = Some(scope.to_proto());
        m.portal_url = self.cfg.portal_url.clone();
        Ok(m)
    }

    /// Records the payment method summary: called by the billing portal integration, not by
    /// account admins (the RPC surface is read only).
    pub async fn set_payment_method(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        input: PaymentMethodInput,
    ) -> Result<BillingPaymentMethod> {
        self.require_enabled()?;
        match input.kind {
            BillingPaymentMethodKind::Card => {
                if input.card_brand.is_empty() || input.card_brand.len() > 30 {
                    return Err(ForgeError::invalid(
                        "invalid_card",
                        "card brand is 1-30 characters",
                    ));
                }
                if input.card_last4.len() != 4
                    || !input.card_last4.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(ForgeError::invalid(
                        "invalid_card",
                        "card_last4 is exactly four digits",
                    ));
                }
                if !(1..=12).contains(&input.card_exp_month)
                    || !(2000..=2200).contains(&input.card_exp_year)
                {
                    return Err(ForgeError::invalid("invalid_card", "bad card expiry"));
                }
            }
            BillingPaymentMethodKind::None | BillingPaymentMethodKind::Invoice => {
                if !input.card_brand.is_empty()
                    || !input.card_last4.is_empty()
                    || input.card_exp_month != 0
                    || input.card_exp_year != 0
                {
                    return Err(ForgeError::invalid(
                        "invalid_card",
                        "card fields are only for card payment methods",
                    ));
                }
            }
            BillingPaymentMethodKind::Unspecified => return Err(ForgeError::missing("kind")),
        }
        if input.billing_name.len() > 200 {
            return Err(ForgeError::invalid(
                "invalid_name",
                "billing name over 200 bytes",
            ));
        }
        if !input.country.is_empty()
            && !(input.country.len() == 2 && input.country.bytes().all(|b| b.is_ascii_uppercase()))
        {
            return Err(ForgeError::invalid(
                "invalid_country",
                "country is an ISO 3166-1 alpha-2 code",
            ));
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let key = scope.key();
        let before = tx
            .get_msg::<BillingPaymentMethod>(&ACCOUNTS, &key, "payment")
            .await?
            .map(|(m, _)| json(&m));
        let m = BillingPaymentMethod {
            scope: None,
            kind: input.kind as i32,
            card_brand: input.card_brand,
            card_last4: input.card_last4,
            card_exp_month: input.card_exp_month,
            card_exp_year: input.card_exp_year,
            billing_name: input.billing_name,
            country: input.country,
            portal_url: String::new(),
            updated_at: Some(ts(tx.now_ms())),
        };
        tx.put(&ACCOUNTS, &Row::new(key.clone(), "payment").msg(&m))
            .await?;
        let mut out = m;
        out.scope = Some(scope.to_proto());
        out.portal_url = self.cfg.portal_url.clone();
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.payment_method_updated",
                action: "billing.payment_method.set",
                target: &key,
                before: before.as_deref(),
                resource: &json(&out),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// The email settings row, or the defaults (receipts and usage alerts on, no address).
    pub(crate) async fn load_email(
        &self,
        tx: &mut Tx,
        scope: &ResolvedScope,
    ) -> Result<BillingEmailSettings> {
        let stored = tx
            .get_msg::<BillingEmailSettings>(&ACCOUNTS, &scope.key(), "email")
            .await?
            .map(|(s, _)| s);
        let mut s = stored.unwrap_or(BillingEmailSettings {
            send_receipts: true,
            send_usage_alerts: true,
            ..Default::default()
        });
        s.scope = Some(scope.to_proto());
        Ok(s)
    }

    /// Where billing email goes.
    pub async fn get_email_settings(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
    ) -> Result<BillingEmailSettings> {
        self.require_enabled()?;
        let mut tx = self.db.begin(SURFACE, cx).await?;
        self.load_email(&mut tx, scope).await
    }

    /// Changes where billing email goes; unset fields are unchanged.
    pub async fn update_email_settings(
        &self,
        cx: &Cx,
        scope: &ResolvedScope,
        req: UpdateBillingEmailSettingsRequest,
    ) -> Result<BillingEmailSettings> {
        self.require_enabled()?;
        if let Some(p) = &req.primary_email
            && !p.is_empty()
            && !valid_email(p)
        {
            return Err(ForgeError::invalid(
                "invalid_email",
                format!("{p:?} is not an email address"),
            ));
        }
        if req.replace_additional_emails {
            if req.additional_emails.len() > MAX_EMAILS {
                return Err(ForgeError::too_large(
                    "additional_emails",
                    MAX_EMAILS as u64,
                    req.additional_emails.len() as u64,
                ));
            }
            if let Some(bad) = req.additional_emails.iter().find(|e| !valid_email(e)) {
                return Err(ForgeError::invalid(
                    "invalid_email",
                    format!("{bad:?} is not an email address"),
                ));
            }
        }
        let mut tx = self.db.begin(SURFACE, cx).await?;
        let key = scope.key();
        let mut s = self.load_email(&mut tx, scope).await?;
        let before = json(&s);
        if let Some(p) = req.primary_email {
            s.primary_email = p;
        }
        if req.replace_additional_emails {
            let mut v = req.additional_emails;
            v.sort();
            v.dedup();
            s.additional_emails = v;
        }
        if let Some(r) = req.send_receipts {
            s.send_receipts = r;
        }
        if let Some(a) = req.send_usage_alerts {
            s.send_usage_alerts = a;
        }
        let mut stored = s.clone();
        stored.scope = None;
        tx.put(&ACCOUNTS, &Row::new(key.clone(), "email").msg(&stored))
            .await?;
        record_change(
            &mut tx,
            scope,
            Change {
                kind: "billing.email_settings_updated",
                action: "billing.email_settings.update",
                target: &key,
                before: Some(&before),
                resource: &json(&s),
                event_org: &event_org(scope),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(s)
    }
}
