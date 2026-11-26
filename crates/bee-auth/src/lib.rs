//! Principals, delegated budgeted tokens, GitHub/OIDC login, service identities.
//!
//! - [`Principal`]: humans, agents (always owned) and services (SPEC §9.1).
//! - Tokens are Biscuit tokens (SPEC §9.2). [`KeyRing::mint`] issues one; [`attenuate`] derives
//!   a narrower one offline; [`Verifier::verify`] checks signature, key id, revocation, expiry
//!   and every block, and returns a [`VerifiedToken`] whose scope is the intersection of all
//!   blocks; [`VerifiedToken::authorize`] checks one request.
//! - Revoking a token id revokes every token derived from it ([`InMemoryRevocations`]).
//! - Key rotation: the first key in a [`KeyRing`] signs, every key verifies.
//! - Budgets are charged per token block and quotas per principal and delegation root through
//!   the [`BudgetLedger`] trait ([`InMemoryLedger`]).
//! - Service secrets are stored as SHA-256 and compared in constant time ([`SecretHash`]).
//!
//! Time is always passed in (`now`, unix seconds); nothing here reads the clock.

mod budget;
mod error;
mod keys;
mod principal;
mod revocation;
mod scope;
mod secret;
mod token;

pub use budget::{BudgetCharge, BudgetKey, BudgetLedger, InMemoryLedger, Usage};
pub use error::{AuthError, AuthReason};
pub use keys::{KeyRing, SigningKey, VerifyingKeys};
pub use principal::{AgentInfo, Principal, PrincipalRef};
pub use revocation::{InMemoryRevocations, RevocationList, TokenId};
pub use scope::{Budget, Request, Scope};
pub use secret::{SecretHash, ServiceCredentials, ServiceSecret};
pub use token::{
    Attenuation, DEFAULT_TOKEN_LIFETIME_SECS, MintRequest, Token, VerifiedToken, Verifier,
    attenuate,
};
