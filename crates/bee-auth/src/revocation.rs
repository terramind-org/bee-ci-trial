//! Revocation by token id.
//!
//! Every Biscuit block has a revocation id (its signature), and a derived token contains every
//! block of its parent. A token's id is the revocation id of its last block, so revoking a
//! token id revokes that token and every token derived from it, while its parent and siblings
//! stay valid.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Mutex;

/// A token id: lower-case hex of a block revocation id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TokenId(pub String);

impl TokenId {
    pub(crate) fn from_bytes(bytes: &[u8]) -> TokenId {
        TokenId(crate::secret::to_hex(bytes))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A source of revocations. The counter feeds the policy epoch (SPEC §9.5) so caches drop
/// authorization decisions when anything is revoked.
pub trait RevocationList {
    /// Whether this revocation id is revoked.
    fn is_revoked(&self, id: &TokenId) -> bool;
    /// Number of revocations so far; increases on every new revocation.
    fn counter(&self) -> u64;
}

/// An in-memory revocation list.
#[derive(Debug, Default)]
pub struct InMemoryRevocations {
    inner: Mutex<(BTreeSet<TokenId>, u64)>,
}

impl InMemoryRevocations {
    /// An empty list.
    pub fn new() -> InMemoryRevocations {
        InMemoryRevocations::default()
    }

    /// Revokes a token id (and with it every derived token). Returns false if it was already
    /// revoked.
    pub fn revoke(&self, id: &TokenId) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let new = g.0.insert(id.clone());
        if new {
            g.1 += 1;
        }
        new
    }
}

impl RevocationList for InMemoryRevocations {
    fn is_revoked(&self, id: &TokenId) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .contains(id)
    }

    fn counter(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).1
    }
}
