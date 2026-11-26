//! Signing keys and rotation: the first key signs, every listed key verifies.

use std::collections::BTreeMap;
use std::fmt;

use biscuit_auth::{Algorithm, KeyPair, PrivateKey, PublicKey};

use crate::error::{AuthError, AuthReason};

/// A root signing key with its key id. The id is written into every token it signs, so the
/// verifier can pick the right public key after a rotation.
pub struct SigningKey {
    id: u32,
    keypair: KeyPair,
}

impl SigningKey {
    /// Generates a fresh Ed25519 key from the operating system's random source.
    pub fn generate(id: u32) -> SigningKey {
        SigningKey {
            id,
            keypair: KeyPair::new(),
        }
    }

    /// Loads an Ed25519 key from its 32-byte secret.
    pub fn from_secret_bytes(id: u32, secret: &[u8; 32]) -> Result<SigningKey, AuthError> {
        let private = PrivateKey::from_bytes(secret, Algorithm::Ed25519).map_err(|e| {
            AuthError::new(AuthReason::InvalidArgument, format!("bad signing key: {e}"))
        })?;
        Ok(SigningKey {
            id,
            keypair: KeyPair::from(&private),
        })
    }

    /// The key id.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The public half.
    pub fn public_key(&self) -> PublicKey {
        self.keypair.public()
    }

    pub(crate) fn keypair(&self) -> &KeyPair {
        &self.keypair
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("id", &self.id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// The server's signing keys. The first key signs new tokens; every key verifies. Rotation:
/// [`KeyRing::rotate`] puts a new key first (old tokens keep verifying), and once every token
/// signed by the old key has expired (default lifetime 12 h), [`KeyRing::retire`] removes it.
#[derive(Debug)]
pub struct KeyRing {
    keys: Vec<SigningKey>,
}

impl KeyRing {
    /// A key ring with one key.
    pub fn new(first: SigningKey) -> KeyRing {
        KeyRing { keys: vec![first] }
    }

    /// Makes `key` the signing key; existing keys keep verifying.
    pub fn rotate(&mut self, key: SigningKey) -> Result<(), AuthError> {
        if self.keys.iter().any(|k| k.id == key.id) {
            return Err(AuthError::new(
                AuthReason::InvalidArgument,
                format!("key id {} already in use", key.id),
            ));
        }
        self.keys.insert(0, key);
        Ok(())
    }

    /// Removes a verification key. The signing key cannot be retired.
    pub fn retire(&mut self, id: u32) -> Result<(), AuthError> {
        match self.keys.iter().position(|k| k.id == id) {
            Some(0) => Err(AuthError::new(
                AuthReason::InvalidArgument,
                "cannot retire the signing key",
            )),
            Some(i) => {
                self.keys.remove(i);
                Ok(())
            }
            None => Err(AuthError::new(
                AuthReason::InvalidArgument,
                format!("no key with id {id}"),
            )),
        }
    }

    /// The key that signs new tokens.
    pub fn signer(&self) -> &SigningKey {
        &self.keys[0]
    }

    /// The public keys, for verifiers that never sign.
    pub fn verifying_keys(&self) -> VerifyingKeys {
        VerifyingKeys {
            keys: self.keys.iter().map(|k| (k.id, k.public_key())).collect(),
        }
    }
}

/// Public keys by key id.
#[derive(Debug, Clone, Default)]
pub struct VerifyingKeys {
    keys: BTreeMap<u32, PublicKey>,
}

impl VerifyingKeys {
    /// An empty set.
    pub fn new() -> VerifyingKeys {
        VerifyingKeys::default()
    }

    /// Adds a public key.
    pub fn insert(&mut self, id: u32, key: PublicKey) {
        self.keys.insert(id, key);
    }

    /// Removes a public key.
    pub fn remove(&mut self, id: u32) {
        self.keys.remove(&id);
    }

    pub(crate) fn get(&self, id: Option<u32>) -> Option<PublicKey> {
        id.and_then(|id| self.keys.get(&id).copied())
    }
}
