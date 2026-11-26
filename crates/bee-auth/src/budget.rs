//! Budget and quota accounting.
//!
//! A request made with a token is charged to every budgeted block in its chain (keyed by the
//! block's revocation id), so a sub-agent's usage also counts against its parent's budget. On
//! top of token budgets, quotas are kept per principal and per delegation root (SPEC §15.4),
//! so a thousand agents under one human share that human's ceiling.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::{AuthError, AuthReason};
use crate::scope::Budget;

/// What a request consumes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Changesets created.
    pub changesets: u64,
    /// Changes landed.
    pub landed: u64,
    /// Bytes uploaded.
    pub upload_bytes: u64,
}

impl Usage {
    /// One changeset.
    pub fn changeset() -> Usage {
        Usage {
            changesets: 1,
            ..Usage::default()
        }
    }

    /// One landed change.
    pub fn land() -> Usage {
        Usage {
            landed: 1,
            ..Usage::default()
        }
    }

    /// Uploaded bytes.
    pub fn upload(bytes: u64) -> Usage {
        Usage {
            upload_bytes: bytes,
            ..Usage::default()
        }
    }

    fn plus(&self, o: &Usage) -> Usage {
        Usage {
            changesets: self.changesets.saturating_add(o.changesets),
            landed: self.landed.saturating_add(o.landed),
            upload_bytes: self.upload_bytes.saturating_add(o.upload_bytes),
        }
    }
}

/// What a charge is counted against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BudgetKey {
    /// A token block, by revocation id (hex).
    Block(String),
    /// A principal's own quota.
    Principal(String),
    /// The quota shared by everything delegated from one root principal.
    DelegationRoot(String),
}

/// One charge: a key and the limit that applies to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetCharge {
    /// What is charged.
    pub key: BudgetKey,
    /// Limit for that key.
    pub limit: Budget,
}

/// Budget accounting. `charge` is atomic: either every charge fits and all are applied, or
/// none is applied and the error names the first exhausted key.
pub trait BudgetLedger {
    /// Applies `usage` to every key in `charges` if every limit allows it.
    fn charge(&self, charges: &[BudgetCharge], usage: Usage) -> Result<(), AuthError>;
    /// Usage recorded so far for a key.
    fn usage(&self, key: &BudgetKey) -> Usage;
}

/// An in-memory ledger.
#[derive(Debug, Default)]
pub struct InMemoryLedger {
    used: Mutex<BTreeMap<BudgetKey, Usage>>,
}

impl InMemoryLedger {
    /// An empty ledger.
    pub fn new() -> InMemoryLedger {
        InMemoryLedger::default()
    }
}

fn over(limit: Option<u64>, value: u64) -> bool {
    limit.is_some_and(|l| value > l)
}

impl BudgetLedger for InMemoryLedger {
    fn charge(&self, charges: &[BudgetCharge], usage: Usage) -> Result<(), AuthError> {
        let mut used = self.used.lock().unwrap_or_else(|p| p.into_inner());
        for c in charges {
            let next = used.get(&c.key).copied().unwrap_or_default().plus(&usage);
            let what = if over(c.limit.max_changesets, next.changesets) {
                Some(("changesets", c.limit.max_changesets))
            } else if over(c.limit.max_landed, next.landed) {
                Some(("landed changes", c.limit.max_landed))
            } else if over(c.limit.max_upload_bytes, next.upload_bytes) {
                Some(("upload bytes", c.limit.max_upload_bytes))
            } else {
                None
            };
            if let Some((what, limit)) = what {
                let who = match &c.key {
                    BudgetKey::Block(id) => format!("token block {}", &id[..id.len().min(16)]),
                    BudgetKey::Principal(p) => format!("principal {p}"),
                    BudgetKey::DelegationRoot(p) => format!("delegation root {p}"),
                };
                return Err(AuthError::new(
                    AuthReason::BudgetExhausted,
                    format!(
                        "budget of {} {what} for {who} is used up",
                        limit.unwrap_or(0)
                    ),
                ));
            }
        }
        for c in charges {
            let e = used.entry(c.key.clone()).or_default();
            *e = e.plus(&usage);
        }
        Ok(())
    }

    fn usage(&self, key: &BudgetKey) -> Usage {
        self.used
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .copied()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_are_atomic() {
        let l = InMemoryLedger::new();
        let a = BudgetCharge {
            key: BudgetKey::Block("a".into()),
            limit: Budget {
                max_changesets: Some(2),
                ..Budget::default()
            },
        };
        let b = BudgetCharge {
            key: BudgetKey::Block("b".into()),
            limit: Budget {
                max_changesets: Some(1),
                ..Budget::default()
            },
        };
        l.charge(&[a.clone(), b.clone()], Usage::changeset())
            .unwrap();
        let e = l
            .charge(&[a.clone(), b.clone()], Usage::changeset())
            .unwrap_err();
        assert_eq!(e.reason, AuthReason::BudgetExhausted);
        // The failed charge did not touch `a`.
        assert_eq!(l.usage(&a.key).changesets, 1);
        l.charge(std::slice::from_ref(&a), Usage::changeset())
            .unwrap();
        assert!(l.charge(&[a], Usage::changeset()).is_err());
        let up = BudgetCharge {
            key: BudgetKey::Principal("p".into()),
            limit: Budget {
                max_upload_bytes: Some(10),
                ..Budget::default()
            },
        };
        l.charge(std::slice::from_ref(&up), Usage::upload(10))
            .unwrap();
        assert!(l.charge(&[up], Usage::upload(1)).is_err());
    }
}
