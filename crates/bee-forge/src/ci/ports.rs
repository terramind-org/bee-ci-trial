//! What CI needs from outside the forge store, as ports the server wires.
//!
//! - [`CiBlobs`] stores the bytes of logs, artifacts and caches; [`MemoryCiBlobs`] for tests
//!   and `--dev`, [`StoreCiBlobs`] over the VCS blob store (`bee_store::BlobStore`).
//! - [`KeyPort`] signs OIDC tokens and attestations; [`HmacKey`] is the reference
//!   implementation (HS256), a deployment wires an asymmetric key and publishes its JWKS.
//! - [`SecretCipher`] is the server key that seals secrets at rest (ChaCha20-Poly1305).
//! - [`UrlSigner`] signs short-lived URLs for log, artifact and cache transfer.
//! - [`CiConfig`] holds the settings.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_core::Ulid;
use bee_proto::v1::{RunnerArch, RunnerImage, RunnerImageSource, RunnerMachineClass, RunnerOs};
use bee_store::{BlobStore, OrgId, StorageClass};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{ForgeError, Result};
use crate::ids::{hex, unhex};

/// Bytes CI keeps outside the row store: log chunks, artifacts, caches.
#[async_trait]
pub trait CiBlobs: Send + Sync {
    /// Stores bytes in the org's namespace and returns a new blob id. Two puts of the same
    /// bytes return different ids, so deleting one never affects the other.
    async fn put(&self, org_id: &str, bytes: Vec<u8>) -> Result<String>;
    /// Reads a blob; `None` when absent.
    async fn get(&self, org_id: &str, id: &str) -> Result<Option<Vec<u8>>>;
    /// Deletes a blob (absent blobs are fine).
    async fn delete(&self, org_id: &str, id: &str) -> Result<()>;
}

/// In-memory blobs.
#[derive(Default)]
pub struct MemoryCiBlobs {
    map: Mutex<HashMap<(String, String), Vec<u8>>>,
    next: Mutex<u64>,
}

impl MemoryCiBlobs {
    /// An empty store.
    pub fn new() -> MemoryCiBlobs {
        MemoryCiBlobs::default()
    }

    /// Number of stored blobs.
    pub fn len(&self) -> usize {
        self.map.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Whether nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every stored blob's bytes (tests use it to prove what is and is not at rest).
    pub fn all_bytes(&self) -> Vec<Vec<u8>> {
        self.map
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
}

#[async_trait]
impl CiBlobs for MemoryCiBlobs {
    async fn put(&self, org_id: &str, bytes: Vec<u8>) -> Result<String> {
        let id = {
            let mut n = self
                .next
                .lock()
                .map_err(|_| ForgeError::internal("blob counter poisoned"))?;
            *n += 1;
            format!("mem{:016}", *n)
        };
        self.map
            .lock()
            .map_err(|_| ForgeError::internal("blob map poisoned"))?
            .insert((org_id.to_string(), id.clone()), bytes);
        Ok(id)
    }

    async fn get(&self, org_id: &str, id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .map
            .lock()
            .map_err(|_| ForgeError::internal("blob map poisoned"))?
            .get(&(org_id.to_string(), id.to_string()))
            .cloned())
    }

    async fn delete(&self, org_id: &str, id: &str) -> Result<()> {
        self.map
            .lock()
            .map_err(|_| ForgeError::internal("blob map poisoned"))?
            .remove(&(org_id.to_string(), id.to_string()));
        Ok(())
    }
}

/// Blobs in the VCS blob store. Each put wraps the bytes in an envelope with 16 random bytes
/// so ids are unique (the store is content-addressed) and deletes are safe.
pub struct StoreCiBlobs {
    store: Arc<dyn BlobStore>,
}

impl StoreCiBlobs {
    /// Over `store`.
    pub fn new(store: Arc<dyn BlobStore>) -> StoreCiBlobs {
        StoreCiBlobs { store }
    }
}

fn org(org_id: &str) -> Result<OrgId> {
    Ulid::parse(org_id)
        .map(OrgId)
        .map_err(|e| ForgeError::invalid("bad_id", e.message))
}

fn blob_err(e: bee_core::BeeError) -> ForgeError {
    ForgeError::unavailable(format!("blob store: {}", e.message))
}

#[async_trait]
impl CiBlobs for StoreCiBlobs {
    async fn put(&self, org_id: &str, bytes: Vec<u8>) -> Result<String> {
        let mut envelope = vec![0u8; 16];
        getrandom::fill(&mut envelope).map_err(|e| ForgeError::internal(format!("rng: {e}")))?;
        envelope.extend_from_slice(&bytes);
        let id = bee_object::blob_id(&envelope);
        self.store
            .put(org(org_id)?, &id, envelope, StorageClass::Hot)
            .await
            .map_err(blob_err)?;
        Ok(id.to_text())
    }

    async fn get(&self, org_id: &str, id: &str) -> Result<Option<Vec<u8>>> {
        let oid =
            bee_core::ObjectId::parse(id).map_err(|e| ForgeError::invalid("bad_id", e.message))?;
        let got = self.store.get(org(org_id)?, &oid).await.map_err(blob_err)?;
        Ok(got.map(|b| b.get(16..).map(<[u8]>::to_vec).unwrap_or_default()))
    }

    async fn delete(&self, org_id: &str, id: &str) -> Result<()> {
        let oid =
            bee_core::ObjectId::parse(id).map_err(|e| ForgeError::invalid("bad_id", e.message))?;
        self.store
            .delete(org(org_id)?, &oid, None)
            .await
            .map_err(blob_err)?;
        Ok(())
    }
}

/// A signing key for OIDC tokens (JWS) and attestation bundles.
pub trait KeyPort: Send + Sync {
    /// Key id (`kid`).
    fn key_id(&self) -> String;
    /// JWS algorithm name (`HS256`, `ES256`, ...).
    fn algorithm(&self) -> &'static str;
    /// Signs bytes.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>>;
    /// Verifies a signature made by [`KeyPort::sign`].
    fn verify(&self, message: &[u8], signature: &[u8]) -> bool;
}

/// HMAC-SHA256 key (`HS256`): the reference [`KeyPort`] for tests and single-node setups
/// where the relying party shares the key.
pub struct HmacKey {
    id: String,
    key: Vec<u8>,
}

impl HmacKey {
    /// A key with id `id`.
    pub fn new(id: impl Into<String>, key: &[u8]) -> HmacKey {
        HmacKey {
            id: id.into(),
            key: key.to_vec(),
        }
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Result<Vec<u8>> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key)
        .map_err(|e| ForgeError::internal(format!("hmac key: {e}")))?;
    mac.update(message);
    Ok(mac.finalize().into_bytes().to_vec())
}

impl KeyPort for HmacKey {
    fn key_id(&self) -> String {
        self.id.clone()
    }

    fn algorithm(&self) -> &'static str {
        "HS256"
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        hmac_sha256(&self.key, message)
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(mut mac) = <Hmac<Sha256> as Mac>::new_from_slice(&self.key) else {
            return false;
        };
        mac.update(message);
        mac.verify_slice(signature).is_ok()
    }
}

/// ECDSA P-256 key (`ES256`): signatures anyone can verify with the public key, which the
/// server publishes in its JWKS (`/.well-known/jwks.json`). Attestation bundles signed with it
/// verify with standard Sigstore/DSSE tooling given that key; Actions OIDC tokens verify with
/// the JWKS. The key id is the first 16 hex digits of SHA-256 over the public point.
pub struct EcdsaP256Key {
    id: String,
    pair: ring::signature::EcdsaKeyPair,
}

impl EcdsaP256Key {
    /// The key in a PKCS#8 document.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<EcdsaP256Key> {
        let pair = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8,
            &ring::rand::SystemRandom::new(),
        )
        .map_err(|_| ForgeError::internal("not a P-256 PKCS#8 key"))?;
        use ring::signature::KeyPair as _;
        let digest = ring::digest::digest(&ring::digest::SHA256, pair.public_key().as_ref());
        let id = bee_core::hex::encode(&digest.as_ref()[..8]);
        Ok(EcdsaP256Key { id, pair })
    }

    /// A fresh key (PKCS#8 document and key).
    pub fn generate() -> Result<(Vec<u8>, EcdsaP256Key)> {
        let doc = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &ring::rand::SystemRandom::new(),
        )
        .map_err(|_| ForgeError::internal("cannot generate a P-256 key"))?;
        let key = EcdsaP256Key::from_pkcs8(doc.as_ref())?;
        Ok((doc.as_ref().to_vec(), key))
    }

    /// The public key, an uncompressed SEC1 point (65 bytes).
    pub fn public_key(&self) -> Vec<u8> {
        use ring::signature::KeyPair as _;
        self.pair.public_key().as_ref().to_vec()
    }
}

impl KeyPort for EcdsaP256Key {
    fn key_id(&self) -> String {
        self.id.clone()
    }

    fn algorithm(&self) -> &'static str {
        "ES256"
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        self.pair
            .sign(&ring::rand::SystemRandom::new(), message)
            .map(|s| s.as_ref().to_vec())
            .map_err(|_| ForgeError::internal("ES256 signing failed"))
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            self.public_key(),
        )
        .verify(message, signature)
        .is_ok()
    }
}

/// Seals secret values at rest with ChaCha20-Poly1305 under a 32-byte server key. The scope
/// and name are bound as associated data, so a ciphertext cannot be moved to another secret.
#[derive(Clone)]
pub struct SecretCipher {
    key: [u8; 32],
}

impl std::fmt::Debug for SecretCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretCipher(..)")
    }
}

impl SecretCipher {
    /// A cipher over `key`.
    pub fn new(key: [u8; 32]) -> SecretCipher {
        SecretCipher { key }
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(&Key::from(self.key))
    }

    /// Encrypts `plaintext` bound to `aad`; returns `(nonce, ciphertext)`.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|e| ForgeError::internal(format!("rng: {e}")))?;
        let ct = self
            .cipher()
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| ForgeError::internal("secret encryption failed"))?;
        Ok((nonce.to_vec(), ct))
    }

    /// Decrypts a value sealed with [`SecretCipher::seal`].
    pub fn open(&self, nonce: &[u8], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce: [u8; 12] = nonce
            .try_into()
            .map_err(|_| ForgeError::new(bee_proto::v1::ErrorReason::Integrity, "bad nonce"))?;
        self.cipher()
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| {
                ForgeError::new(
                    bee_proto::v1::ErrorReason::Integrity,
                    "secret ciphertext does not authenticate under the server key",
                )
            })
    }
}

/// Signs short-lived URLs (`<base><path>?expires=<ms>&signature=<hex>`) with HMAC-SHA256.
#[derive(Clone)]
pub struct UrlSigner {
    base: String,
    key: Vec<u8>,
}

impl std::fmt::Debug for UrlSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UrlSigner")
            .field("base", &self.base)
            .finish()
    }
}

impl UrlSigner {
    /// A signer for URLs under `base` (no trailing slash).
    pub fn new(base: impl Into<String>, key: &[u8]) -> UrlSigner {
        UrlSigner {
            base: base.into().trim_end_matches('/').to_string(),
            key: key.to_vec(),
        }
    }

    fn mac(&self, path: &str, expires_ms: u64) -> String {
        hex(&hmac_sha256(&self.key, format!("{path}\n{expires_ms}").as_bytes()).unwrap_or_default())
    }

    /// The signed URL of `path` (starting with `/`) valid until `expires_ms`.
    pub fn sign(&self, path: &str, expires_ms: u64) -> String {
        format!(
            "{}{}?expires={}&signature={}",
            self.base,
            path,
            expires_ms,
            self.mac(path, expires_ms)
        )
    }

    /// Checks a signed URL (full URL or path with query) at `now_ms`; returns its path.
    pub fn verify(&self, url: &str, now_ms: u64) -> Result<String> {
        let rest = url.strip_prefix(&self.base).unwrap_or(url);
        let (path, query) = rest
            .split_once('?')
            .ok_or_else(|| ForgeError::permission_denied("unsigned URL"))?;
        let mut expires = None;
        let mut sig = None;
        for kv in query.split('&') {
            match kv.split_once('=') {
                Some(("expires", v)) => expires = v.parse::<u64>().ok(),
                Some(("signature", v)) => sig = unhex(v),
                _ => {}
            }
        }
        let (Some(expires), Some(sig)) = (expires, sig) else {
            return Err(ForgeError::permission_denied("unsigned URL"));
        };
        if now_ms >= expires {
            return Err(ForgeError::new(
                bee_proto::v1::ErrorReason::TokenExpired,
                "signed URL expired",
            ));
        }
        let Ok(mut mac) = <Hmac<Sha256> as Mac>::new_from_slice(&self.key) else {
            return Err(ForgeError::internal("hmac key"));
        };
        mac.update(format!("{path}\n{expires}").as_bytes());
        mac.verify_slice(&sig)
            .map_err(|_| ForgeError::permission_denied("bad URL signature"))?;
        Ok(path.to_string())
    }
}

/// CI settings.
#[derive(Clone, Debug)]
pub struct CiConfig {
    /// Web URL of the forge (`github.server_url`).
    pub server_url: String,
    /// REST API URL (`github.api_url`).
    pub api_url: String,
    /// GraphQL URL (`github.graphql_url`).
    pub graphql_url: String,
    /// OIDC issuer (`iss`).
    pub oidc_issuer: String,
    /// Job lease length; a runner renews before it ends.
    pub lease_ms: u64,
    /// Keep-alive interval of `AcquireRunnerJob` streams.
    pub keepalive_ms: u64,
    /// Requeues after lost leases before the job fails.
    pub max_requeues: u32,
    /// Registration token lifetime.
    pub registration_token_ttl_ms: u64,
    /// Signed URL lifetime.
    pub signed_url_ttl_ms: u64,
    /// OIDC token lifetime.
    pub oidc_ttl_ms: u64,
    /// Default artifact retention.
    pub artifact_retention_days: u32,
    /// Default log retention.
    pub log_retention_days: u32,
    /// Default cache size limit per repo.
    pub cache_max_bytes: u64,
    /// Default cache retention after last access.
    pub cache_retention_days: u32,
    /// How long after completion a run can be re-run.
    pub rerun_window_days: u32,
    /// A runner not seen for this long is offline.
    pub runner_offline_ms: u64,
    /// Hosted machine classes offered to `runs-on`.
    pub machine_classes: Vec<RunnerMachineClass>,
    /// Platform runner images.
    pub platform_images: Vec<RunnerImage>,
    /// Billing period start for usage reports (ms).
    pub billing_period_start_ms: u64,
    /// How often a job held by a `require_green` environment re-checks the green status of
    /// its changeset.
    pub green_poll_ms: u64,
}

fn class(
    id: &str,
    os: RunnerOs,
    arch: RunnerArch,
    cores: u32,
    usd_micros: u64,
) -> RunnerMachineClass {
    RunnerMachineClass {
        id: id.into(),
        os: os as i32,
        arch: arch as i32,
        cpu_cores: cores,
        memory_bytes: u64::from(cores) * 4 * 1024 * 1024 * 1024,
        storage_bytes: u64::from(cores) * 37 * 1024 * 1024 * 1024,
        gpu: String::new(),
        gpu_count: 0,
        per_minute_usd_micros: usd_micros,
        static_ip_available: cores >= 4,
    }
}

fn image(id: &str, name: &str, os: RunnerOs, arch: RunnerArch) -> RunnerImage {
    RunnerImage {
        id: id.into(),
        name: name.into(),
        source: RunnerImageSource::Platform as i32,
        os: os as i32,
        arch: arch as i32,
        ..Default::default()
    }
}

impl Default for CiConfig {
    fn default() -> CiConfig {
        CiConfig {
            server_url: "https://beehive.localhost".into(),
            api_url: "https://beehive.localhost/api/v3".into(),
            graphql_url: "https://beehive.localhost/api/graphql".into(),
            oidc_issuer: "https://beehive.localhost/_services/token".into(),
            lease_ms: 120_000,
            keepalive_ms: 15_000,
            max_requeues: 3,
            registration_token_ttl_ms: 3_600_000,
            signed_url_ttl_ms: 600_000,
            oidc_ttl_ms: 300_000,
            artifact_retention_days: 90,
            log_retention_days: 90,
            cache_max_bytes: 10 * 1024 * 1024 * 1024,
            cache_retention_days: 7,
            rerun_window_days: 30,
            runner_offline_ms: 120_000,
            machine_classes: vec![
                class("linux-x64-2", RunnerOs::Linux, RunnerArch::X64, 2, 8_000),
                class("linux-x64-4", RunnerOs::Linux, RunnerArch::X64, 4, 16_000),
                class("linux-x64-8", RunnerOs::Linux, RunnerArch::X64, 8, 32_000),
                class(
                    "linux-arm64-2",
                    RunnerOs::Linux,
                    RunnerArch::Arm64,
                    2,
                    5_000,
                ),
                class(
                    "linux-arm64-8",
                    RunnerOs::Linux,
                    RunnerArch::Arm64,
                    8,
                    20_000,
                ),
                class(
                    "macos-arm64-3",
                    RunnerOs::Macos,
                    RunnerArch::Arm64,
                    3,
                    62_000,
                ),
                class(
                    "windows-x64-2",
                    RunnerOs::Windows,
                    RunnerArch::X64,
                    2,
                    16_000,
                ),
            ],
            platform_images: vec![
                image(
                    "ubuntu-24.04",
                    "ubuntu-24.04",
                    RunnerOs::Linux,
                    RunnerArch::X64,
                ),
                image(
                    "ubuntu-24.04-arm",
                    "ubuntu-24.04-arm",
                    RunnerOs::Linux,
                    RunnerArch::Arm64,
                ),
                image("macos-15", "macos-15", RunnerOs::Macos, RunnerArch::Arm64),
                image(
                    "windows-2025",
                    "windows-2025",
                    RunnerOs::Windows,
                    RunnerArch::X64,
                ),
            ],
            billing_period_start_ms: 0,
            green_poll_ms: 10_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cipher_round_trips_and_binds_aad() {
        let c = SecretCipher::new([7u8; 32]);
        let (n, ct) = c.seal(b"hunter2", b"repo:x/TOKEN").unwrap();
        assert!(!ct.windows(7).any(|w| w == b"hunter2"));
        assert_eq!(c.open(&n, &ct, b"repo:x/TOKEN").unwrap(), b"hunter2");
        assert!(c.open(&n, &ct, b"repo:y/TOKEN").is_err());
        assert!(
            SecretCipher::new([8u8; 32])
                .open(&n, &ct, b"repo:x/TOKEN")
                .is_err()
        );
    }

    #[test]
    fn signed_urls_expire_and_reject_tampering() {
        let s = UrlSigner::new("https://h", b"k");
        let url = s.sign("/a/b", 1000);
        assert_eq!(s.verify(&url, 999).unwrap(), "/a/b");
        assert!(s.verify(&url, 1000).is_err());
        assert!(s.verify(&url.replace("/a/b", "/a/c"), 10).is_err());
        assert!(s.verify("/a/b", 10).is_err());
    }

    #[test]
    fn hmac_key_signs_and_verifies() {
        let k = HmacKey::new("k1", b"secret");
        let sig = k.sign(b"m").unwrap();
        assert!(k.verify(b"m", &sig));
        assert!(!k.verify(b"n", &sig));
    }
}
