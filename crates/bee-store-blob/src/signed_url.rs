//! Signed object URLs (SPEC §10.5).
//!
//! When a client sets `allow_signed_urls` on GetObjects, the server may answer an object with
//! a URL instead of its bytes, after the same authorization check as a direct read. A
//! [`SignedUrlIssuer`] makes the URL; the rules of §10.5 are enforced here, so every
//! implementation obeys them:
//!
//! - one URL names one object of one org;
//! - it expires after the requested lifetime, which is clamped to the issuer's maximum
//!   ([`DEFAULT_SIGNED_URL_TTL`], 60 s) and is at least one second;
//! - it is never issued for bytes that are not servable: absent or quarantined (and
//!   therefore not for redacted objects, whose bytes are purged; the server must also
//!   refuse redacted ids itself before calling the issuer, because the blob store cannot
//!   know about redaction rows);
//! - the client verifies the downloaded bytes against the object id exactly as for
//!   streamed bytes ([`crate::signed_url::bytes_match_id`]).
//!
//! # Implementations
//!
//! - [`S3UrlIssuer`]: a SigV4 presigned GET on the S3-compatible store, fetched by the client
//!   from the bucket or the CDN in front of it directly.
//! - [`LocalUrlIssuer`]: for servers without a CDN, an HMAC-SHA256 signed URL served by
//!   `bee-server` itself.
//!
//! # The local route
//!
//! ```text
//! GET <base_url>/blob/<org ULID>/<object id text>?exp=<unix seconds>&sig=<64 hex digits>
//! ```
//!
//! `sig` is HMAC-SHA256, keyed with the server's URL key, over
//! `"swarm-blob-url-v1\n" ‖ org ‖ "\n" ‖ id ‖ "\n" ‖ exp` (the same text as in the path and
//! query). The server mounts a handler on that route that does the following, in order:
//!
//! 1. calls [`verify_local_url`] (or [`LocalUrlIssuer::verify`]) with the two path segments
//!    and the two query values; any error maps to its reason (`UNAUTHENTICATED` for a bad or
//!    expired signature, `INVALID_ARGUMENT` for a malformed org or id);
//! 2. reads the bytes with [`bee_store::BlobStore::get`] (which re-hashes them and never
//!    serves quarantined bytes), answering 404 for `None`;
//! 3. sends them with `Cache-Control: private, max-age=<remaining seconds>`; the object is
//!    immutable but the URL is a capability, so shared caches must not keep it past its expiry.
//!
//! The handler does not authorize: possession of a valid, unexpired URL is the capability, and
//! the authorization check happened when the URL was issued. Key rotation: give the verifier
//! the new key first and the previous key second; URLs live under a minute, so the previous key
//! can be dropped a minute later.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bee_core::{BeeError, ObjectId, Reason, Result, Subcode, Ulid, hex};
use bee_store::blob::blob_storable;
use bee_store::{BlobStore, Clock, OrgId};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::s3::S3BlobStore;

/// The default (and, per SPEC §10.5, default maximum) URL lifetime: `signed_url_ttl`.
pub const DEFAULT_SIGNED_URL_TTL: Duration = Duration::from_secs(60);

/// Shortest key accepted for local URLs.
const MIN_KEY_LEN: usize = 32;
/// Domain separation of the local URL MAC.
const MAC_DOMAIN: &str = "swarm-blob-url-v1";

/// An issued URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedUrl {
    /// Where to GET the object's bytes.
    pub url: String,
    /// When the URL stops working, in Unix milliseconds.
    pub expires_at_ms: u64,
}

/// Issues signed URLs for immutable object reads. See the [module docs](self) for the rules
/// every implementation follows.
#[async_trait]
pub trait SignedUrlIssuer: Send + Sync {
    /// A URL for the bytes of `id` in `org`, valid for `ttl` (clamped to the issuer's maximum).
    ///
    /// Errors: `INVALID_ARGUMENT` for a `ttl` under one second or an id the blob store never
    /// holds, `NOT_FOUND` when the bytes are absent or quarantined.
    async fn issue(&self, org: OrgId, id: &ObjectId, ttl: Duration) -> Result<SignedUrl>;
}

/// True when `bytes` hash to `id`: the check a client makes on bytes fetched through a
/// signed URL.
pub fn bytes_match_id(id: &ObjectId, bytes: &[u8]) -> bool {
    bee_object::hash_id(id.kind(), bytes) == *id
}

fn check_ttl(ttl: Duration, max_ttl: Duration) -> Result<Duration> {
    if ttl < Duration::from_secs(1) {
        return Err(BeeError::invalid(
            Subcode::InvalidField,
            "a signed URL lives at least one second",
        ));
    }
    Ok(ttl.min(max_ttl))
}

async fn require_servable(store: &dyn BlobStore, org: OrgId, id: &ObjectId) -> Result<()> {
    if !blob_storable(id.kind()) {
        return Err(BeeError::invalid(
            Subcode::WrongKind,
            format!("{id} is never stored as blob-store bytes"),
        ));
    }
    if store.has(org, std::slice::from_ref(id)).await?.first() != Some(&true) {
        return Err(BeeError::new(
            Reason::NotFound,
            format!("no servable bytes for {id}; no URL issued"),
        ));
    }
    Ok(())
}

/// Presigned GET URLs on an [`S3BlobStore`].
#[derive(Clone, Debug)]
pub struct S3UrlIssuer {
    store: S3BlobStore,
    max_ttl: Duration,
}

impl S3UrlIssuer {
    /// An issuer over `store` with the default 60 s maximum lifetime.
    pub fn new(store: S3BlobStore) -> S3UrlIssuer {
        S3UrlIssuer {
            store,
            max_ttl: DEFAULT_SIGNED_URL_TTL,
        }
    }

    /// Sets the maximum lifetime (`signed_url_ttl`).
    pub fn with_max_ttl(mut self, max_ttl: Duration) -> S3UrlIssuer {
        self.max_ttl = max_ttl;
        self
    }
}

#[async_trait]
impl SignedUrlIssuer for S3UrlIssuer {
    async fn issue(&self, org: OrgId, id: &ObjectId, ttl: Duration) -> Result<SignedUrl> {
        let ttl = check_ttl(ttl, self.max_ttl)?;
        require_servable(&self.store, org, id).await?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let secs = ttl.as_secs();
        let url = self
            .store
            .presign_get_at(org, id, Duration::from_secs(secs), now_ms)?;
        Ok(SignedUrl {
            url,
            expires_at_ms: now_ms / 1000 * 1000 + secs * 1000,
        })
    }
}

/// HMAC-signed URLs served by `bee-server` itself. See the [module docs](self) for the
/// route.
pub struct LocalUrlIssuer {
    base_url: String,
    keys: Vec<Vec<u8>>,
    store: Arc<dyn BlobStore>,
    clock: Arc<dyn Clock>,
    max_ttl: Duration,
}

impl std::fmt::Debug for LocalUrlIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalUrlIssuer")
            .field("base_url", &self.base_url)
            .field("keys", &self.keys.len())
            .field("max_ttl", &self.max_ttl)
            .finish()
    }
}

impl LocalUrlIssuer {
    /// An issuer whose URLs start with `base_url` (the server's public origin, no trailing
    /// slash needed), signed with `keys[0]`; all of `keys` verify. Every key is at least 32
    /// bytes. The maximum lifetime is the default 60 s.
    pub fn new(
        base_url: impl Into<String>,
        keys: Vec<Vec<u8>>,
        store: Arc<dyn BlobStore>,
        clock: Arc<dyn Clock>,
    ) -> Result<LocalUrlIssuer> {
        if keys.is_empty() || keys.iter().any(|k| k.len() < MIN_KEY_LEN) {
            return Err(BeeError::invalid(
                Subcode::InvalidField,
                "signed URL keys must be at least 32 bytes and at least one is required",
            ));
        }
        Ok(LocalUrlIssuer {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            keys,
            store,
            clock,
            max_ttl: DEFAULT_SIGNED_URL_TTL,
        })
    }

    /// Sets the maximum lifetime (`signed_url_ttl`).
    pub fn with_max_ttl(mut self, max_ttl: Duration) -> LocalUrlIssuer {
        self.max_ttl = max_ttl;
        self
    }

    /// The URL for `id` expiring at `exp_secs` (Unix seconds), signed with the current key.
    pub fn url_for(&self, org: OrgId, id: &ObjectId, exp_secs: u64) -> String {
        let sig = hex::encode(&mac(
            &self.keys[0],
            &org.0.to_text(),
            &id.to_text(),
            exp_secs,
        ));
        format!(
            "{}/blob/{}/{}?exp={exp_secs}&sig={sig}",
            self.base_url,
            org.0.to_text(),
            id.to_text()
        )
    }

    /// Verifies the parts of a request to the local route; see [`verify_local_url`]. Uses
    /// this issuer's keys, clock and maximum lifetime.
    pub fn verify(&self, org: &str, id: &str, exp: &str, sig: &str) -> Result<(OrgId, ObjectId)> {
        let keys: Vec<&[u8]> = self.keys.iter().map(Vec::as_slice).collect();
        verify_local_url(&keys, self.clock.now_ms(), self.max_ttl, org, id, exp, sig)
    }
}

#[async_trait]
impl SignedUrlIssuer for LocalUrlIssuer {
    async fn issue(&self, org: OrgId, id: &ObjectId, ttl: Duration) -> Result<SignedUrl> {
        let ttl = check_ttl(ttl, self.max_ttl)?;
        require_servable(self.store.as_ref(), org, id).await?;
        let expires_ms = self.clock.now_ms() + ttl.as_millis() as u64;
        // Whole seconds, rounded down, so the URL never outlives the requested lifetime.
        let exp_secs = expires_ms / 1000;
        Ok(SignedUrl {
            url: self.url_for(org, id, exp_secs),
            expires_at_ms: exp_secs * 1000,
        })
    }
}

fn mac(key: &[u8], org: &str, id: &str, exp_secs: u64) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    m.update(format!("{MAC_DOMAIN}\n{org}\n{id}\n{exp_secs}").as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// Verifies a request to `GET /blob/<org>/<id>?exp=<secs>&sig=<hex>` and returns the org and
/// object it grants.
///
/// `keys` are the accepted signing keys (current first); `now_ms` is the verifier's clock.
/// The signature is compared in constant time. Errors:
///
/// - `INVALID_ARGUMENT` for an `org` or `id` that does not parse, or an `exp` that is not a
///   decimal number;
/// - `UNAUTHENTICATED` for a signature that is malformed or wrong, a URL past its `exp`, or
///   one whose `exp` is more than `max_ttl` ahead (no honest issuer makes those).
pub fn verify_local_url(
    keys: &[&[u8]],
    now_ms: u64,
    max_ttl: Duration,
    org: &str,
    id: &str,
    exp: &str,
    sig: &str,
) -> Result<(OrgId, ObjectId)> {
    let org_id = OrgId(Ulid::parse(org)?);
    let object = ObjectId::parse(id)?;
    let exp_secs: u64 = exp
        .parse()
        .map_err(|_| BeeError::invalid(Subcode::InvalidField, "exp is not a number of seconds"))?;
    let denied = |why: &str| BeeError::new(Reason::Unauthenticated, format!("signed URL {why}"));
    let sig_bytes = hex::decode(sig)
        .filter(|b| b.len() == 32)
        .ok_or_else(|| denied("has a malformed signature"))?;
    let (org_text, id_text) = (org_id.0.to_text(), object.to_text());
    let signed = keys.iter().any(|key| {
        let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
        m.update(format!("{MAC_DOMAIN}\n{org_text}\n{id_text}\n{exp_secs}").as_bytes());
        m.verify_slice(&sig_bytes).is_ok()
    });
    if !signed {
        return Err(denied("signature does not match"));
    }
    let expires_ms = exp_secs.saturating_mul(1000);
    if expires_ms <= now_ms {
        return Err(denied("has expired"));
    }
    if expires_ms - now_ms > max_ttl.as_millis() as u64 + 1000 {
        return Err(denied("expires later than any URL is issued for"));
    }
    Ok((org_id, object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bee_store::{ManualClock, MemoryBlobStore, StorageClass};

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn org() -> OrgId {
        OrgId(Ulid::from_u128(9))
    }

    async fn setup() -> (
        LocalUrlIssuer,
        Arc<MemoryBlobStore>,
        Arc<ManualClock>,
        ObjectId,
    ) {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let store = Arc::new(MemoryBlobStore::new(clock.clone()));
        let id = bee_object::blob_id(b"hello");
        store
            .put(org(), &id, b"hello".to_vec(), StorageClass::Hot)
            .await
            .unwrap();
        let issuer = LocalUrlIssuer::new(
            "https://hub.example/",
            vec![KEY.to_vec()],
            store.clone(),
            clock.clone(),
        )
        .unwrap();
        (issuer, store, clock, id)
    }

    fn parts(url: &str) -> (String, String, String, String) {
        let rest = url.split("/blob/").nth(1).unwrap();
        let (path, query) = rest.split_once('?').unwrap();
        let (o, i) = path.split_once('/').unwrap();
        let (exp, sig) = query.split_once("&sig=").unwrap();
        (
            o.to_string(),
            i.to_string(),
            exp.strip_prefix("exp=").unwrap().to_string(),
            sig.to_string(),
        )
    }

    #[tokio::test]
    async fn issued_urls_verify_until_they_expire() {
        let (issuer, _, clock, id) = setup().await;
        let url = issuer
            .issue(org(), &id, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(url.url.starts_with("https://hub.example/blob/"));
        assert_eq!(url.expires_at_ms, 1_060_000);
        let (o, i, exp, sig) = parts(&url.url);
        assert_eq!(issuer.verify(&o, &i, &exp, &sig).unwrap(), (org(), id));
        clock.set(1_059_999);
        assert!(issuer.verify(&o, &i, &exp, &sig).is_ok());
        clock.set(1_060_000);
        let err = issuer.verify(&o, &i, &exp, &sig).unwrap_err();
        assert_eq!(err.reason, Reason::Unauthenticated);
    }

    #[tokio::test]
    async fn ttl_is_clamped_and_must_be_positive() {
        let (issuer, _, _, id) = setup().await;
        let long = issuer
            .issue(org(), &id, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(long.expires_at_ms, 1_060_000);
        let err = issuer
            .issue(org(), &id, Duration::from_millis(500))
            .await
            .unwrap_err();
        assert_eq!(err.reason, Reason::InvalidArgument);
    }

    #[tokio::test]
    async fn tampering_is_rejected() {
        let (issuer, _, _, id) = setup().await;
        let url = issuer
            .issue(org(), &id, Duration::from_secs(30))
            .await
            .unwrap();
        let (o, i, exp, sig) = parts(&url.url);
        let other = bee_object::blob_id(b"other").to_text();
        let other_org = OrgId(Ulid::from_u128(10)).0.to_text();
        let later = (exp.parse::<u64>().unwrap() + 1).to_string();
        let mut bad_sig = sig.clone();
        bad_sig.replace_range(..1, if sig.starts_with('0') { "1" } else { "0" });
        for (o, i, e, s) in [
            (o.as_str(), other.as_str(), exp.as_str(), sig.as_str()),
            (other_org.as_str(), i.as_str(), exp.as_str(), sig.as_str()),
            (o.as_str(), i.as_str(), later.as_str(), sig.as_str()),
            (o.as_str(), i.as_str(), exp.as_str(), bad_sig.as_str()),
            (o.as_str(), i.as_str(), exp.as_str(), "zz"),
            (o.as_str(), i.as_str(), exp.as_str(), ""),
        ] {
            assert_eq!(
                issuer.verify(o, i, e, s).unwrap_err().reason,
                Reason::Unauthenticated,
                "{o} {i} {e} {s}"
            );
        }
        assert_eq!(
            issuer.verify("nope", &i, &exp, &sig).unwrap_err().reason,
            Reason::InvalidArgument
        );
        assert_eq!(
            issuer.verify(&o, &i, "soon", &sig).unwrap_err().reason,
            Reason::InvalidArgument
        );
        // A different key does not verify.
        let wrong: &[u8] = b"ffffffffffffffffffffffffffffffff";
        assert!(
            verify_local_url(
                &[wrong],
                1_000_000,
                DEFAULT_SIGNED_URL_TTL,
                &o,
                &i,
                &exp,
                &sig
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn far_future_expiry_is_rejected_even_when_signed() {
        let (issuer, _, _, id) = setup().await;
        let url = issuer.url_for(org(), &id, 1_000_000 / 1000 + 3600);
        let (o, i, exp, sig) = parts(&url);
        assert_eq!(
            issuer.verify(&o, &i, &exp, &sig).unwrap_err().reason,
            Reason::Unauthenticated
        );
    }

    #[tokio::test]
    async fn never_issued_for_quarantined_or_absent_bytes() {
        let (issuer, store, _, id) = setup().await;
        store.quarantine(org(), &id).await.unwrap();
        let err = issuer
            .issue(org(), &id, Duration::from_secs(60))
            .await
            .unwrap_err();
        assert_eq!(err.reason, Reason::NotFound);
        let missing = bee_object::blob_id(b"missing");
        assert_eq!(
            issuer
                .issue(org(), &missing, Duration::from_secs(60))
                .await
                .unwrap_err()
                .reason,
            Reason::NotFound
        );
        store.release(org(), &id).await.unwrap();
        assert!(
            issuer
                .issue(org(), &id, Duration::from_secs(60))
                .await
                .is_ok()
        );
        let changeset = bee_object::hash_id(bee_core::ObjectKind::Changeset, b"c");
        assert_eq!(
            issuer
                .issue(org(), &changeset, Duration::from_secs(60))
                .await
                .unwrap_err()
                .reason,
            Reason::InvalidArgument
        );
    }

    #[tokio::test]
    async fn key_rotation_accepts_the_previous_key() {
        let (issuer, store, clock, id) = setup().await;
        let url = issuer
            .issue(org(), &id, Duration::from_secs(30))
            .await
            .unwrap();
        let rotated = LocalUrlIssuer::new(
            "https://hub.example",
            vec![b"new-key-new-key-new-key-new-key-!!".to_vec(), KEY.to_vec()],
            store,
            clock,
        )
        .unwrap();
        let (o, i, exp, sig) = parts(&url.url);
        assert!(rotated.verify(&o, &i, &exp, &sig).is_ok());
        assert!(
            LocalUrlIssuer::new(
                "x",
                vec![b"short".to_vec()],
                Arc::new(MemoryBlobStore::new(Arc::new(ManualClock::new(0)))),
                Arc::new(ManualClock::new(0))
            )
            .is_err()
        );
    }

    #[test]
    fn clients_check_downloaded_bytes() {
        let id = bee_object::blob_id(b"abc");
        assert!(bytes_match_id(&id, b"abc"));
        assert!(!bytes_match_id(&id, b"abd"));
    }
}
