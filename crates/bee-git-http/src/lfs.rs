//! Git LFS served natively: the batch API with `basic` transfers through signed URLs, backed by
//! the blob store.
//!
//! - `POST <repo>.git/info/lfs/objects/batch` authenticates like git (Basic or Bearer), needs
//!   read access for `download` and write access for `upload`, and answers each object with a
//!   signed `href` on this server (or a per-object error).
//! - `PUT <repo>.git/info/lfs/objects/<oid>?…&sig=` stores the bytes after checking their
//!   SHA-256 and size: they become an ordinary bee blob (deduplicated per org, uploaded by
//!   the signing principal) and an `lfs_object` derived row maps the LFS id to it.
//! - `GET <repo>.git/info/lfs/objects/<oid>?…&sig=` streams the blob back.
//!
//! A signature is HMAC-SHA256 over the method, repo, object id, size, expiry and principal
//! with the server's URL key; it expires after [`URL_TTL_SECS`].

use bee_core::{ObjectId, Reason};
use bee_history::{BlobContent, History};
use bee_store::{DerivedRow, UploadRecord};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::backend::RepoSession;
use crate::error::{GitError, GitResult};

/// Derived type mapping an LFS object id to the bee blob holding its bytes.
pub const LFS_OBJECT_TYPE: &str = bee_store::gc::LFS_OBJECT_TYPE;
/// Version of [`LFS_OBJECT_TYPE`].
pub const LFS_OBJECT_VERSION: u32 = 1;
/// Lifetime of a signed transfer URL.
pub const URL_TTL_SECS: u64 = 3600;
/// The LFS media type.
pub const LFS_MEDIA_TYPE: &str = "application/vnd.git-lfs+json";

/// One object of a batch request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BatchObject {
    /// SHA-256 of the content, lowercase hex.
    pub oid: String,
    /// Size in bytes.
    pub size: u64,
}

/// A batch request.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct BatchRequest {
    /// `download` or `upload`.
    pub operation: String,
    /// Transfer adapters the client supports.
    #[serde(default)]
    pub transfers: Vec<String>,
    /// The objects.
    pub objects: Vec<BatchObject>,
    /// Hash algorithm (only `sha256`).
    #[serde(default)]
    pub hash_algo: Option<String>,
}

/// The key of an object's `lfs_object` row.
pub fn lfs_key(oid: &[u8; 32]) -> GitResult<ObjectId> {
    Ok(ObjectId::git_foreign(oid)?)
}

/// Parses a 64-hex-digit LFS object id.
pub fn parse_oid(text: &str) -> GitResult<[u8; 32]> {
    let bytes = bee_core::hex::decode(text)
        .filter(|b| b.len() == 32 && bee_core::hex::is_lower_hex(text))
        .ok_or_else(|| GitError::invalid(format!("{text:?} is not an LFS sha256 object id")))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` escapes.
pub fn decode_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The parameters of a signed transfer URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    /// `GET` or `PUT`.
    pub method: String,
    /// `owner/repo`.
    pub repo: String,
    /// The object id (hex).
    pub oid: String,
    /// The size.
    pub size: u64,
    /// Expiry, unix seconds.
    pub exp: u64,
    /// The principal the URL was issued to.
    pub principal: String,
}

impl Signed {
    fn message(&self) -> Vec<u8> {
        format!(
            "swarm-lfs\0{}\0{}\0{}\0{}\0{}\0{}",
            self.method, self.repo, self.oid, self.size, self.exp, self.principal
        )
        .into_bytes()
    }

    fn mac(key: &[u8]) -> GitResult<Hmac<Sha256>> {
        Hmac::<Sha256>::new_from_slice(key).map_err(|_| GitError::internal("bad URL key"))
    }

    /// The signature (hex).
    pub fn sign(&self, key: &[u8]) -> GitResult<String> {
        let mut m = Self::mac(key)?;
        m.update(&self.message());
        Ok(bee_core::hex::encode(&m.finalize().into_bytes()))
    }

    /// Checks a signature (constant time) and the expiry.
    pub fn verify(&self, key: &[u8], sig: &str, now_secs: u64) -> GitResult<()> {
        let bad = || GitError::unauthenticated("the LFS transfer URL is invalid or expired");
        let raw = bee_core::hex::decode(sig).ok_or_else(bad)?;
        let mut m = Self::mac(key)?;
        m.update(&self.message());
        m.verify_slice(&raw).map_err(|_| bad())?;
        if now_secs >= self.exp {
            return Err(bad());
        }
        Ok(())
    }

    /// The URL path and query under `base` (`http://host:port`).
    pub fn url(&self, base: &str, key: &[u8]) -> GitResult<String> {
        let (owner, name) = self.repo.split_once('/').unwrap_or((&self.repo, ""));
        Ok(format!(
            "{}/{}/{}.git/info/lfs/objects/{}?size={}&exp={}&p={}&sig={}",
            base.trim_end_matches('/'),
            encode_component(owner),
            encode_component(name),
            self.oid,
            self.size,
            self.exp,
            encode_component(&self.principal),
            self.sign(key)?
        ))
    }
}

async fn blob_of(history: &History, oid: &[u8; 32]) -> GitResult<Option<ObjectId>> {
    match history
        .derived(&lfs_key(oid)?, LFS_OBJECT_TYPE, LFS_OBJECT_VERSION)
        .await?
    {
        Some(v) => Ok(Some(ObjectId::from_bytes(&v)?)),
        None => Ok(None),
    }
}

/// Answers a batch request.
pub async fn batch(
    session: &dyn RepoSession,
    req: &BatchRequest,
    base: &str,
    key: &[u8],
    now_secs: u64,
) -> GitResult<serde_json::Value> {
    if req.hash_algo.as_deref().is_some_and(|h| h != "sha256") {
        return Err(GitError::invalid(
            "only the sha256 LFS hash algorithm is supported",
        ));
    }
    if !req.transfers.is_empty() && !req.transfers.iter().any(|t| t == "basic") {
        return Err(GitError::invalid(
            "only the basic LFS transfer adapter is supported",
        ));
    }
    let upload = match req.operation.as_str() {
        "upload" => true,
        "download" => false,
        other => {
            return Err(GitError::invalid(format!(
                "unknown LFS operation {other:?}"
            )));
        }
    };
    let history = session.history();
    let mut objects = Vec::with_capacity(req.objects.len());
    for o in &req.objects {
        let oid = match parse_oid(&o.oid) {
            Ok(b) => b,
            Err(e) => {
                objects.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 422, "message": e.line() } }));
                continue;
            }
        };
        let present = match blob_of(history, &oid).await? {
            Some(id) => history
                .meta()
                .get_object(history.org(), &id, bee_store::ReadFrom::Primary)
                .await?
                .is_some_and(|r| r.state == bee_store::ObjectState::Live && r.size == o.size),
            None => false,
        };
        if upload && o.size > session.max_blob_bytes() {
            objects.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 422,
                "message": format!("TOO_LARGE: objects over {} bytes are refused", session.max_blob_bytes()) } }));
            continue;
        }
        if !upload && !present {
            objects.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 404, "message": "NOT_FOUND: object not found" } }));
            continue;
        }
        if upload && present {
            objects.push(json!({ "oid": o.oid, "size": o.size, "authenticated": true }));
            continue;
        }
        let s = Signed {
            method: if upload { "PUT" } else { "GET" }.into(),
            repo: session.full_name().to_string(),
            oid: o.oid.clone(),
            size: o.size,
            exp: now_secs + URL_TTL_SECS,
            principal: session.principal().to_string(),
        };
        let action = if upload { "upload" } else { "download" };
        objects.push(json!({
            "oid": o.oid, "size": o.size, "authenticated": true,
            "actions": { action: { "href": s.url(base, key)?, "expires_in": URL_TTL_SECS } },
        }));
    }
    Ok(json!({ "transfer": "basic", "objects": objects, "hash_algo": "sha256" }))
}

/// Stores an uploaded LFS object after verifying its id and size.
pub async fn put(
    history: &History,
    signed: &Signed,
    body: Vec<u8>,
    max_bytes: u64,
    at_ms: u64,
) -> GitResult<()> {
    let oid = parse_oid(&signed.oid)?;
    if body.len() as u64 != signed.size {
        return Err(GitError::invalid(format!(
            "LFS object is {} bytes, the batch said {}",
            body.len(),
            signed.size
        )));
    }
    if signed.size > max_bytes {
        return Err(GitError::new(Reason::TooLarge, "LFS object is too large"));
    }
    let digest: [u8; 32] = Sha256::digest(&body).into();
    if digest != oid {
        return Err(GitError::new(
            Reason::Integrity,
            "LFS object bytes do not hash to its id",
        ));
    }
    let id = bee_object::blob_id(&body);
    history
        .writer()
        .upload(
            history.org(),
            vec![(id, body)],
            Some(UploadRecord {
                repo: Some(history.repo()),
                principal: signed.principal.clone(),
            }),
            at_ms,
        )
        .await?;
    history
        .put_derived(vec![DerivedRow {
            key: lfs_key(&oid)?,
            type_name: LFS_OBJECT_TYPE.into(),
            version: LFS_OBJECT_VERSION,
            value: id.as_bytes().to_vec(),
        }])
        .await?;
    Ok(())
}

/// Reads a stored LFS object.
pub async fn get(history: &History, signed: &Signed) -> GitResult<Vec<u8>> {
    let oid = parse_oid(&signed.oid)?;
    let id = blob_of(history, &oid)
        .await?
        .ok_or_else(|| GitError::not_found("LFS object not found"))?;
    match history.blob(&id).await? {
        BlobContent::Bytes(b) => Ok(b),
        BlobContent::Redacted => Err(GitError::new(Reason::Redacted, "LFS object is redacted")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_bind_every_field_and_expire() {
        let key = [7u8; 32];
        let s = Signed {
            method: "PUT".into(),
            repo: "dev/r".into(),
            oid: "ab".repeat(32),
            size: 5,
            exp: 100,
            principal: "u1".into(),
        };
        let sig = s.sign(&key).unwrap();
        s.verify(&key, &sig, 99).unwrap();
        assert!(s.verify(&key, &sig, 100).is_err(), "expired");
        let mut t = s.clone();
        t.size = 6;
        assert!(t.verify(&key, &sig, 1).is_err(), "size is signed");
        let mut t = s.clone();
        t.method = "GET".into();
        assert!(t.verify(&key, &sig, 1).is_err(), "method is signed");
        assert!(s.verify(&[8u8; 32], &sig, 1).is_err(), "key matters");
        assert!(s.verify(&key, "zz", 1).is_err());
        let url = s.url("http://h:1/", &key).unwrap();
        assert!(url.starts_with(&format!(
            "http://h:1/dev/r.git/info/lfs/objects/{}?size=5&exp=100&p=u1&sig=",
            s.oid
        )));
    }

    #[test]
    fn oids_and_escapes() {
        assert!(parse_oid(&"ab".repeat(32)).is_ok());
        assert!(parse_oid(&"AB".repeat(32)).is_err());
        assert!(parse_oid("abc").is_err());
        assert_eq!(encode_component("agent:x/y"), "agent%3Ax%2Fy");
        assert_eq!(decode_component("agent%3Ax%2Fy"), "agent:x/y");
        assert_eq!(decode_component("100%"), "100%");
    }
}
