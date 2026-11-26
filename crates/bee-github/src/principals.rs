//! Placeholder principals for GitHub accounts, and claims (SPEC §17.2).
//!
//! Every GitHub account that authored, reviewed, commented, reacted, released or was
//! assigned gets a **placeholder principal** keyed by its numeric GitHub id (stable across
//! renames). The principal id is a pure function of that id:
//! ULID(time 0, BLAKE3(`"swarm-github-user\0"` ‖ decimal id)\[..10\]), so the same person
//! gets the same placeholder in every imported repo and one claim covers all of them.
//! Bots (`type: Bot` or a `[bot]` login) become agent principals.
//!
//! A person claims a placeholder by proving they own the GitHub account: the server
//! verifies a GitHub login and passes the verified numeric id
//! ([`ClaimProof::VerifiedGithubId`]), or the person presents the claim token the import
//! issued for that account ([`ClaimProof::Token`]), which is an HMAC of the id under a secret
//! held in the import's state and never written to a report.

use std::collections::BTreeMap;

use bee_core::Ulid;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::error::{Result, denied, internal, not_found};
use crate::model::User;

/// The external-id prefix recorded on placeholder principals.
pub const EXTERNAL_PREFIX: &str = "github:";

/// A placeholder principal.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Placeholder {
    /// GitHub numeric id.
    pub github_id: u64,
    /// Login when imported.
    pub login: String,
    /// Display name.
    pub display: String,
    /// The principal id (ULID text).
    pub principal: String,
    /// True for bots (agent principals).
    pub agent: bool,
}

impl Placeholder {
    /// The placeholder for a GitHub account.
    pub fn of(user: &User) -> Placeholder {
        Placeholder {
            github_id: user.id,
            login: user.login.clone(),
            display: user
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| user.login.clone()),
            principal: principal_id(user.id),
            agent: user.is_bot(),
        }
    }

    /// `github:<id>`.
    pub fn external_id(&self) -> String {
        format!("{EXTERNAL_PREFIX}{}", self.github_id)
    }

    /// `human` or `agent`.
    pub fn kind(&self) -> &'static str {
        if self.agent { "agent" } else { "human" }
    }

    /// The forge principal record.
    pub fn principal_proto(&self) -> bee_proto::v1::Principal {
        bee_proto::v1::Principal {
            id: self.principal.clone(),
            kind: if self.agent {
                bee_proto::v1::PrincipalKind::Agent as i32
            } else {
                bee_proto::v1::PrincipalKind::Human as i32
            },
            display: self.display.clone(),
            external_ids: vec![self.external_id(), format!("github-login:{}", self.login)],
            ..Default::default()
        }
    }
}

/// The deterministic principal id of a GitHub account.
pub fn principal_id(github_id: u64) -> String {
    let h = blake3::hash(format!("swarm-github-user\0{github_id}").as_bytes());
    let mut r = [0u8; 16];
    r[6..].copy_from_slice(&h.as_bytes()[..10]);
    Ulid::from_bytes(r).to_text()
}

/// How a claimant proves they own a GitHub account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimProof {
    /// The server verified a GitHub login for this numeric id.
    VerifiedGithubId(u64),
    /// The claim token the import issued for the account.
    Token(String),
}

/// A recorded claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The placeholder principal id.
    pub placeholder: String,
    /// GitHub numeric id.
    pub github_id: u64,
    /// The principal that now owns the history.
    pub principal: String,
}

/// Placeholders of one import and their claims.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    secret: Vec<u8>,
    by_id: BTreeMap<u64, Placeholder>,
    claims: BTreeMap<u64, String>,
}

impl Directory {
    /// A directory whose claim tokens derive from `secret`, with existing `claims`.
    pub fn new(secret: Vec<u8>, claims: BTreeMap<u64, String>) -> Directory {
        Directory {
            secret,
            by_id: BTreeMap::new(),
            claims,
        }
    }

    /// A fresh random claim secret (hex), for a new state directory.
    pub fn new_secret() -> Result<String> {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(|e| internal(format!("no system randomness: {e}")))?;
        Ok(hex(&b))
    }

    /// Adds every account in `users`.
    pub fn add_all<'a>(&mut self, users: impl IntoIterator<Item = &'a User>) {
        for u in users {
            self.by_id.entry(u.id).or_insert_with(|| Placeholder::of(u));
        }
    }

    /// The placeholder of an account, adding it if new.
    pub fn of(&mut self, user: &User) -> Placeholder {
        self.by_id
            .entry(user.id)
            .or_insert_with(|| Placeholder::of(user))
            .clone()
    }

    /// Every placeholder, by GitHub id.
    pub fn all(&self) -> impl Iterator<Item = &Placeholder> {
        self.by_id.values()
    }

    /// A placeholder by GitHub id.
    pub fn get(&self, github_id: u64) -> Option<&Placeholder> {
        self.by_id.get(&github_id)
    }

    /// The principal that acts for an account: the claimant when claimed, else the
    /// placeholder.
    pub fn acting(&self, github_id: u64) -> Option<String> {
        self.claims
            .get(&github_id)
            .cloned()
            .or_else(|| self.by_id.get(&github_id).map(|p| p.principal.clone()))
    }

    /// True when claimed.
    pub fn claimed(&self, github_id: u64) -> bool {
        self.claims.contains_key(&github_id)
    }

    /// Claims recorded (GitHub id → principal).
    pub fn claims(&self) -> &BTreeMap<u64, String> {
        &self.claims
    }

    /// The claim token for an account (hex, 32 chars). Hand it to the person; it is not
    /// stored anywhere else.
    pub fn claim_token(&self, github_id: u64) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.secret)
            .unwrap_or_else(|_| <Hmac<Sha256> as Mac>::new_from_slice(b"swarm").expect("key"));
        mac.update(format!("claim\0{github_id}").as_bytes());
        hex(&mac.finalize().into_bytes()[..16])
    }

    /// Claims the placeholder of `github_id` for `principal`.
    ///
    /// Fails with `NOT_FOUND` for an account this import never saw, `PERMISSION_DENIED`
    /// when the proof does not match the account, and `ALREADY_EXISTS` when someone else
    /// already claimed it (claiming again for the same principal is a no-op).
    pub fn claim(&mut self, github_id: u64, proof: &ClaimProof, principal: &str) -> Result<Claim> {
        let p = self
            .by_id
            .get(&github_id)
            .ok_or_else(|| not_found(format!("no placeholder for GitHub user {github_id}")))?
            .clone();
        let ok = match proof {
            ClaimProof::VerifiedGithubId(id) => *id == github_id,
            ClaimProof::Token(t) => {
                let want = self.claim_token(github_id);
                constant_time_eq(t.trim().as_bytes(), want.as_bytes())
            }
        };
        if !ok {
            return Err(denied(format!(
                "the proof does not show ownership of GitHub user {github_id}"
            )));
        }
        if let Some(cur) = self.claims.get(&github_id)
            && cur != principal
        {
            return Err(bee_core::BeeError::new(
                bee_core::Reason::AlreadyExists,
                format!("GitHub user {github_id} was already claimed by another principal"),
            ));
        }
        self.claims.insert(github_id, principal.to_string());
        Ok(Claim {
            placeholder: p.principal,
            github_id,
            principal: principal.to_string(),
        })
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Lowercase hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Bytes of lowercase or uppercase hex (`None` when malformed).
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: u64, login: &str) -> User {
        User {
            id,
            login: login.into(),
            kind: "User".into(),
            name: None,
        }
    }

    #[test]
    fn principal_ids_are_stable_and_distinct() {
        assert_eq!(principal_id(42), principal_id(42));
        assert_ne!(principal_id(42), principal_id(43));
        let id = principal_id(42);
        assert_eq!(id.len(), 26);
        assert!(id.starts_with("000000"), "time part is zero: {id}");
        assert!(Ulid::parse(&id).is_ok());
    }

    #[test]
    fn claims_need_proof() {
        let mut d = Directory::new(b"secret".to_vec(), BTreeMap::new());
        d.add_all([&user(7, "alice")]);
        let tok = d.claim_token(7);
        assert_eq!(tok.len(), 32);
        assert!(d.claim(7, &ClaimProof::Token("nope".into()), "p1").is_err());
        assert!(d.claim(7, &ClaimProof::VerifiedGithubId(8), "p1").is_err());
        assert!(d.claim(9, &ClaimProof::VerifiedGithubId(9), "p1").is_err());
        let c = d.claim(7, &ClaimProof::Token(tok), "p1").unwrap();
        assert_eq!(c.placeholder, principal_id(7));
        assert_eq!(d.acting(7).as_deref(), Some("p1"));
        // Same principal again: fine. Someone else: refused.
        assert!(d.claim(7, &ClaimProof::VerifiedGithubId(7), "p1").is_ok());
        let e = d
            .claim(7, &ClaimProof::VerifiedGithubId(7), "p2")
            .unwrap_err();
        assert_eq!(e.reason, bee_core::Reason::AlreadyExists);
    }

    #[test]
    fn tokens_depend_on_the_secret() {
        let a = Directory::new(b"a".to_vec(), BTreeMap::new());
        let b = Directory::new(b"b".to_vec(), BTreeMap::new());
        assert_ne!(a.claim_token(1), b.claim_token(1));
        assert_eq!(unhex(&hex(&[1, 255])).unwrap(), vec![1, 255]);
    }
}
