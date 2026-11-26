//! Service secrets: stored only as SHA-256, compared in constant time, never logged.

use std::collections::BTreeMap;
use std::fmt;

use bee_policy::PrincipalId;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{AuthError, AuthReason};

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[usize::from(b >> 4)] as char);
        s.push(HEX[usize::from(b & 0x0f)] as char);
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    s.as_bytes()
        .chunks(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

/// A secret presented by a client. Its `Debug` output is redacted so it cannot leak into logs.
pub struct ServiceSecret(String);

impl ServiceSecret {
    /// Wraps a presented secret.
    pub fn new(secret: impl Into<String>) -> ServiceSecret {
        ServiceSecret(secret.into())
    }

    /// The SHA-256 to store for this secret.
    pub fn hash(&self) -> SecretHash {
        SecretHash::of(self.0.as_bytes())
    }
}

impl fmt::Debug for ServiceSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServiceSecret(<redacted>)")
    }
}

/// The stored form of a service secret: its SHA-256.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretHash([u8; 32]);

impl SecretHash {
    /// Hashes a secret.
    pub fn of(secret: &[u8]) -> SecretHash {
        SecretHash(Sha256::digest(secret).into())
    }

    /// Parses 64 lower-case hex digits.
    pub fn from_hex(s: &str) -> Result<SecretHash, AuthError> {
        let bytes = from_hex(s).filter(|b| b.len() == 32).ok_or_else(|| {
            AuthError::new(
                AuthReason::InvalidArgument,
                "secret hash must be 64 lower-case hex digits",
            )
        })?;
        let mut a = [0u8; 32];
        a.copy_from_slice(&bytes);
        Ok(SecretHash(a))
    }

    /// Lower-case hex.
    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    /// Whether `presented` hashes to this value, compared in constant time.
    pub fn matches(&self, presented: &ServiceSecret) -> bool {
        let h: [u8; 32] = Sha256::digest(presented.0.as_bytes()).into();
        bool::from(self.0.ct_eq(&h))
    }
}

impl fmt::Debug for SecretHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretHash(<redacted>)")
    }
}

/// Registered service credentials: name → principal and secret hash.
#[derive(Debug, Default)]
pub struct ServiceCredentials {
    by_name: BTreeMap<String, (PrincipalId, SecretHash)>,
}

impl ServiceCredentials {
    /// An empty registry.
    pub fn new() -> ServiceCredentials {
        ServiceCredentials::default()
    }

    /// Registers (or replaces) a credential.
    pub fn register(&mut self, name: impl Into<String>, principal: PrincipalId, hash: SecretHash) {
        self.by_name.insert(name.into(), (principal, hash));
    }

    /// Removes a credential.
    pub fn remove(&mut self, name: &str) {
        self.by_name.remove(name);
    }

    /// Checks a presented secret. Unknown names still pay for one hash comparison, so timing
    /// does not reveal which names exist.
    pub fn authenticate(
        &self,
        name: &str,
        secret: &ServiceSecret,
    ) -> Result<PrincipalId, AuthError> {
        let fail = || {
            AuthError::new(
                AuthReason::Unauthenticated,
                "unknown service or wrong secret",
            )
        };
        match self.by_name.get(name) {
            Some((principal, hash)) if hash.matches(secret) => Ok(principal.clone()),
            Some(_) => Err(fail()),
            None => {
                let _ = SecretHash([0u8; 32]).matches(secret);
                Err(fail())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_hashed_and_redacted() {
        let secret = ServiceSecret::new("s3cr3t");
        let hash = secret.hash();
        assert_eq!(hash.to_hex().len(), 64);
        assert_eq!(SecretHash::from_hex(&hash.to_hex()).unwrap(), hash);
        assert!(SecretHash::from_hex("zz").is_err());
        assert!(!format!("{secret:?}").contains("s3cr3t"));
        assert!(!format!("{hash:?}").contains(&hash.to_hex()));
        // Known vector: SHA-256("abc").
        assert_eq!(
            SecretHash::of(b"abc").to_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn authenticate() {
        let mut creds = ServiceCredentials::new();
        creds.register(
            "ci",
            PrincipalId::new("S1"),
            ServiceSecret::new("right").hash(),
        );
        assert_eq!(
            creds
                .authenticate("ci", &ServiceSecret::new("right"))
                .unwrap(),
            PrincipalId::new("S1")
        );
        let e = creds
            .authenticate("ci", &ServiceSecret::new("wrong"))
            .unwrap_err();
        assert_eq!(e.reason, AuthReason::Unauthenticated);
        assert!(
            creds
                .authenticate("nope", &ServiceSecret::new("right"))
                .is_err()
        );
        creds.remove("ci");
        assert!(
            creds
                .authenticate("ci", &ServiceSecret::new("right"))
                .is_err()
        );
    }
}
