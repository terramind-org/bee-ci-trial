//! Client-visible authentication and authorization errors.

use std::fmt;

/// SPEC §12.2 reason codes produced by this crate. None of them is retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuthReason {
    /// The token or credential is malformed, has a bad signature, names an unknown signing
    /// key, or the service secret does not match.
    Unauthenticated,
    /// The token's expiry has passed.
    TokenExpired,
    /// The token or one of the tokens it was derived from was revoked.
    TokenRevoked,
    /// The request is outside the token's scope.
    PolicyDenied,
    /// A budget in the token chain, or a quota of the delegation root, is used up.
    BudgetExhausted,
    /// A mint or attenuation request is malformed (for example an agent without an owner).
    InvalidArgument,
}

impl AuthReason {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthReason::Unauthenticated => "UNAUTHENTICATED",
            AuthReason::TokenExpired => "TOKEN_EXPIRED",
            AuthReason::TokenRevoked => "TOKEN_REVOKED",
            AuthReason::PolicyDenied => "POLICY_DENIED",
            AuthReason::BudgetExhausted => "BUDGET_EXHAUSTED",
            AuthReason::InvalidArgument => "INVALID_ARGUMENT",
        }
    }
}

impl fmt::Display for AuthReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An authentication or authorization failure: a stable reason and a human-readable message.
/// Messages never contain secrets or token bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthError {
    /// Stable reason.
    pub reason: AuthReason,
    /// Explanation.
    pub message: String,
}

impl AuthError {
    pub(crate) fn new(reason: AuthReason, message: impl Into<String>) -> AuthError {
        AuthError {
            reason,
            message: message.into(),
        }
    }

    /// Whether a client may retry the same request unchanged. Always false for these reasons.
    pub fn retryable(&self) -> bool {
        false
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for AuthError {}
