//! Sealed boxes for secret values (libsodium `crypto_box_seal` semantics).
//!
//! `SecretService.PutSecret` carries a secret value sealed by the client with the scope's
//! X25519 public key: `ephemeral_pk (32) || crypto_box(m, nonce, recipient_pk, ephemeral_sk)`
//! where the nonce is BLAKE2b-192 of `ephemeral_pk || recipient_pk` and `crypto_box` is
//! X25519 + XSalsa20-Poly1305 with the 16-byte tag first. Any libsodium binding
//! (`crypto_box_seal`, PyNaCl `SealedBox`, tweetsodium) produces boxes this module opens.
//!
//! - [`SealedBoxPort`] is what the server needs: the current public key of a scope and the
//!   ability to open a box made for it. [`DerivedSealKeys`] is the reference implementation:
//!   each scope's private key is derived from one 32-byte master secret, so nothing extra is
//!   stored and every server node agrees. A deployment may wire a KMS instead.
//! - [`seal`], [`seal_with_ephemeral`] and [`open`] are the client-side primitives (the CLI and
//!   tests use them); [`seal_with_ephemeral`] is deterministic for known-answer vectors.
//!
//! Opened values are re-sealed at rest with the server cipher ([`super::SecretCipher`]) by
//! [`super::Ci::put_secret`]; sealed boxes are never stored.

use async_trait::async_trait;
use crypto_box::aead::rand_core::{CryptoRng, Error as RngError, RngCore};
use crypto_box::{PublicKey, SecretKey};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::{ForgeError, Result};
use crate::ids::hex;

/// Bytes a sealed box adds to its plaintext (ephemeral key and tag).
pub const SEAL_OVERHEAD: usize = crypto_box::SEALBYTES;

/// The public half of a scope's sealing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealKey {
    /// Opaque id the client passes back with the sealed value.
    pub key_id: String,
    /// The 32-byte X25519 public key.
    pub public_key: [u8; 32],
}

/// What the server needs to accept sealed secret values.
#[async_trait]
pub trait SealedBoxPort: Send + Sync {
    /// The current key of a secret scope (`VaultScope::key`), creating it on first use.
    async fn public_key(&self, scope_key: &str) -> Result<SealKey>;
    /// Opens `sealed`, made with key `key_id` of `scope_key`. Errors: INVALID_ARGUMENT with
    /// subcode `stale_key_id` (not the scope's current key) or `bad_ciphertext`.
    async fn open(&self, scope_key: &str, key_id: &str, sealed: &[u8]) -> Result<Vec<u8>>;
}

/// [`SealedBoxPort`] deriving each scope's X25519 private key from a master secret with
/// HMAC-SHA256 (`bee.ci.sealed-box.v1`, scope key, generation). Rotating means raising the
/// generation; boxes sealed for an older generation are then refused as `stale_key_id`.
#[derive(Clone)]
pub struct DerivedSealKeys {
    master: [u8; 32],
    generation: u32,
}

impl std::fmt::Debug for DerivedSealKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DerivedSealKeys")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl DerivedSealKeys {
    /// Keys derived from `master`, generation 1.
    pub fn new(master: [u8; 32]) -> DerivedSealKeys {
        DerivedSealKeys {
            master,
            generation: 1,
        }
    }

    /// The same keys at `generation` (rotation).
    pub fn with_generation(mut self, generation: u32) -> DerivedSealKeys {
        self.generation = generation.max(1);
        self
    }

    fn secret(&self, scope_key: &str) -> Result<SecretKey> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.master)
            .map_err(|e| ForgeError::internal(format!("seal key: {e}")))?;
        mac.update(format!("swarm.ci.sealed-box.v1\n{scope_key}\n{}", self.generation).as_bytes());
        let bytes: [u8; 32] = mac.finalize().into_bytes().into();
        Ok(SecretKey::from_bytes(bytes))
    }

    fn key_of(&self, sk: &SecretKey) -> SealKey {
        let public_key = sk.public_key().to_bytes();
        let digest = Sha256::digest(public_key);
        SealKey {
            key_id: format!("{}-{}", self.generation, hex(&digest[..8])),
            public_key,
        }
    }
}

#[async_trait]
impl SealedBoxPort for DerivedSealKeys {
    async fn public_key(&self, scope_key: &str) -> Result<SealKey> {
        Ok(self.key_of(&self.secret(scope_key)?))
    }

    async fn open(&self, scope_key: &str, key_id: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        let sk = self.secret(scope_key)?;
        let current = self.key_of(&sk);
        if key_id != current.key_id {
            return Err(ForgeError::invalid(
                "stale_key_id",
                format!(
                    "key {key_id:?} is not the current key of this scope ({}); fetch the public key again",
                    current.key_id
                ),
            ));
        }
        sk.unseal(sealed).map_err(|_| bad_ciphertext())
    }
}

fn bad_ciphertext() -> ForgeError {
    ForgeError::invalid(
        "bad_ciphertext",
        "the value is not a sealed box for this scope's public key",
    )
}

/// Bytes from the operating system's generator.
struct OsBytes;

impl RngCore for OsBytes {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }

    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        // A failing OS generator leaves zeros; `seal` checks for that and refuses.
        if getrandom::fill(dest).is_err() {
            dest.fill(0);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), RngError> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for OsBytes {}

/// A generator that yields one fixed 32-byte ephemeral key (deterministic sealing).
struct Fixed([u8; 32]);

impl RngCore for Fixed {
    fn next_u32(&mut self) -> u32 {
        u32::from_le_bytes([self.0[0], self.0[1], self.0[2], self.0[3]])
    }

    fn next_u64(&mut self) -> u64 {
        u64::from(self.next_u32())
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for (d, s) in dest.iter_mut().zip(self.0.iter().cycle()) {
            *d = *s;
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), RngError> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for Fixed {}

/// Seals `plaintext` for `public_key` with a fresh ephemeral key (`crypto_box_seal`).
pub fn seal(public_key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut eph = [0u8; 32];
    OsBytes.fill_bytes(&mut eph);
    if eph == [0u8; 32] {
        return Err(ForgeError::internal(
            "the operating system's random generator failed",
        ));
    }
    seal_with_ephemeral(public_key, eph, plaintext)
}

/// Seals `plaintext` for `public_key` with the given ephemeral secret key. Deterministic, so
/// known-answer vectors can be checked; production sealing uses [`seal`].
pub fn seal_with_ephemeral(
    public_key: &[u8; 32],
    ephemeral_secret: [u8; 32],
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    PublicKey::from_bytes(*public_key)
        .seal(&mut Fixed(ephemeral_secret), plaintext)
        .map_err(|_| ForgeError::internal("sealing failed"))
}

/// The X25519 public key of a 32-byte secret key.
pub fn public_key_of(secret_key: [u8; 32]) -> [u8; 32] {
    SecretKey::from_bytes(secret_key).public_key().to_bytes()
}

/// Opens a sealed box with the recipient's secret key (`crypto_box_seal_open`).
pub fn open(secret_key: [u8; 32], sealed: &[u8]) -> Result<Vec<u8>> {
    SecretKey::from_bytes(secret_key)
        .unseal(sealed)
        .map_err(|_| bad_ciphertext())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_round_trips_and_rejects_tampering() {
        let sk = [5u8; 32];
        let pk = public_key_of(sk);
        let boxed = seal(&pk, b"hunter2").unwrap();
        assert_eq!(boxed.len(), 7 + SEAL_OVERHEAD);
        assert_eq!(open(sk, &boxed).unwrap(), b"hunter2");
        let mut bad = boxed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(open(sk, &bad).is_err());
        assert!(open([6u8; 32], &boxed).is_err());
        assert!(open(sk, &boxed[..20]).is_err());
    }

    #[test]
    fn deterministic_sealing_is_stable() {
        let pk = public_key_of([1u8; 32]);
        let a = seal_with_ephemeral(&pk, [2u8; 32], b"x").unwrap();
        let b = seal_with_ephemeral(&pk, [2u8; 32], b"x").unwrap();
        assert_eq!(a, b);
        assert_eq!(&a[..32], &public_key_of([2u8; 32]));
    }

    #[tokio::test]
    async fn derived_keys_are_per_scope_and_check_the_key_id() {
        let keys = DerivedSealKeys::new([9u8; 32]);
        let a = keys.public_key("repo:a").await.unwrap();
        let b = keys.public_key("repo:b").await.unwrap();
        assert_ne!(a.public_key, b.public_key);
        assert_eq!(a, keys.public_key("repo:a").await.unwrap());
        let boxed = seal(&a.public_key, b"v").unwrap();
        assert_eq!(keys.open("repo:a", &a.key_id, &boxed).await.unwrap(), b"v");
        let e = keys.open("repo:a", "1-00", &boxed).await.unwrap_err();
        assert_eq!(e.subcode, "stale_key_id");
        let e = keys.open("repo:b", &b.key_id, &boxed).await.unwrap_err();
        assert_eq!(e.subcode, "bad_ciphertext");
        let rotated = keys.clone().with_generation(2);
        let e = rotated.open("repo:a", &a.key_id, &boxed).await.unwrap_err();
        assert_eq!(e.subcode, "stale_key_id");
    }
}
