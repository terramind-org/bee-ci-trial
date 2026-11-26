//! Push-protection decisions.

use bee_core::{BeeError, PayloadValue, Reason};

use crate::scanner::Finding;

/// Why a principal bypasses push protection. Mirrors the GitHub bypass reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BypassReason {
    /// The finding is not a real secret.
    FalsePositive,
    /// The secret is only used in tests.
    UsedInTests,
    /// The secret is real; it will be rotated and removed later.
    WillFixLater,
}

impl BypassReason {
    /// Every reason, in declaration order.
    pub const ALL: [BypassReason; 3] = [
        BypassReason::FalsePositive,
        BypassReason::UsedInTests,
        BypassReason::WillFixLater,
    ];

    /// The stable audit string.
    pub fn as_str(self) -> &'static str {
        match self {
            BypassReason::FalsePositive => "false_positive",
            BypassReason::UsedInTests => "used_in_tests",
            BypassReason::WillFixLater => "will_fix_later",
        }
    }

    /// Parses the stable string.
    pub fn parse(text: &str) -> Option<BypassReason> {
        BypassReason::ALL.into_iter().find(|r| r.as_str() == text)
    }
}

/// A request to bypass push protection, already authorised by policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bypass {
    /// The principal that asked for (and was granted) the bypass; must be non-empty.
    pub principal: String,
    /// The stated reason.
    pub reason: BypassReason,
}

/// The outcome of push protection for one push, changeset or land.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    /// True when the operation must be refused with `SECRET_DETECTED`.
    pub blocked: bool,
    /// Every finding, including advisory and bypassed ones, for display and audit.
    pub findings: Vec<Finding>,
    /// Set when a bypass was exercised (blocking findings existed and the bypass was valid);
    /// the caller writes it to the audit log with the findings.
    pub bypass: Option<Bypass>,
}

impl Decision {
    /// The `SECRET_DETECTED` error for a blocked decision, `None` otherwise. The payload lists
    /// `count`, the sorted distinct `rules`, and per-finding `path`, `rule`, `line` and
    /// `preview`; it never contains secret text beyond the redacted preview.
    pub fn to_error(&self) -> Option<BeeError> {
        if !self.blocked {
            return None;
        }
        let blocking: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| f.confidence.blocks())
            .collect();
        let mut rules: Vec<&str> = blocking.iter().map(|f| f.rule_id.as_str()).collect();
        rules.sort_unstable();
        rules.dedup();
        let items = blocking
            .iter()
            .map(|f| {
                let mut m = std::collections::BTreeMap::new();
                m.insert("path".to_owned(), PayloadValue::from(f.path.as_str()));
                m.insert("rule".to_owned(), PayloadValue::from(f.rule_id.as_str()));
                m.insert("line".to_owned(), PayloadValue::Int(f.line as i64));
                m.insert(
                    "preview".to_owned(),
                    PayloadValue::from(f.redacted_preview.as_str()),
                );
                PayloadValue::Map(m)
            })
            .collect();
        Some(
            BeeError::new(
                Reason::SecretDetected,
                format!(
                    "{} secret(s) detected ({}); remove them or request a push-protection bypass",
                    blocking.len(),
                    rules.join(", ")
                ),
            )
            .with_payload("count", PayloadValue::Int(blocking.len() as i64))
            .with_payload(
                "rules",
                PayloadValue::List(rules.iter().map(|r| PayloadValue::from(*r)).collect()),
            )
            .with_payload("findings", PayloadValue::List(items)),
        )
    }
}

/// Decides push protection.
///
/// Only findings whose confidence blocks (`Medium` and `High`) can block. With no blocking
/// findings the result is unblocked and `bypass` is `None`. With blocking findings and a
/// bypass whose principal is non-blank, the result is unblocked and carries the bypass for
/// audit; a blank principal is treated as no bypass. The function is pure.
pub fn evaluate_push_protection(findings: Vec<Finding>, bypass: Option<Bypass>) -> Decision {
    let has_blocking = findings.iter().any(|f| f.confidence.blocks());
    let valid = bypass.filter(|b| !b.principal.trim().is_empty());
    match (has_blocking, valid) {
        (false, _) => Decision {
            blocked: false,
            findings,
            bypass: None,
        },
        (true, Some(b)) => Decision {
            blocked: false,
            findings,
            bypass: Some(b),
        },
        (true, None) => Decision {
            blocked: true,
            findings,
            bypass: None,
        },
    }
}
