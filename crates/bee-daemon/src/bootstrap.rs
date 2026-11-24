//! Daemon bootstrap from a verified snapshot (PRODUCT §4.5, `docs/spec/cache-tiers.md` §6).
//!
//! The daemon asks the tier above for `GetDaemonSnapshot` of a repo scope, downloads the
//! `SWSN` bundle from its signed URL, checks the file digest, the trailer and every object
//! against its id, and only then loads the objects into its cache. A bootstrap moves bytes
//! only: every caller still needs its own decision from the tier above before any of those
//! objects is served to it, so an object redacted after the snapshot was built is refused by
//! the tier above (and evicted) the first time anyone asks for it.

use std::time::Instant;

use bee_core::ObjectId;
use bee_proto::v1::{self, ErrorReason};
use serde::{Deserialize, Serialize};

use crate::error::{Result, err, unavailable};
use crate::snapshot::repo_ref;
use crate::store::verify;
use crate::tier::Tier;
use crate::upstream::{Auth, with_retries};

/// Magic of a snapshot file.
pub const MAGIC: &[u8; 4] = b"SWSN";
/// Format version.
pub const VERSION: u32 = 1;

/// What a bootstrap loaded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapReport {
    /// Snapshot id (the bundle's blob id).
    pub snapshot: String,
    /// Changeset the snapshot was built from.
    pub changeset: String,
    /// Ref and version it followed.
    pub r#ref: String,
    /// Ref version.
    pub ref_version: u64,
    /// Objects in the snapshot.
    pub objects: u64,
    /// Objects that were not cached yet.
    pub loaded: u64,
    /// Bundle size in bytes.
    pub bytes: u64,
    /// Wall time.
    pub millis: f64,
}

/// Parses and verifies an `SWSN` bundle: magic, version, trailer, and every object against
/// its id. Nothing is returned unless all of it verifies.
pub fn decode(file: &[u8]) -> Result<Vec<(ObjectId, &[u8])>> {
    let bad = |why: &str| err(ErrorReason::Integrity, format!("snapshot rejected: {why}"));
    if file.len() < 16 + 32 {
        return Err(bad("too short"));
    }
    let (body, trailer) = file.split_at(file.len() - 32);
    if bee_object::digest(body) != trailer {
        return Err(bad("trailer does not match"));
    }
    if &body[..4] != MAGIC {
        return Err(bad("not an SWSN file"));
    }
    if body[4..8] != VERSION.to_le_bytes() {
        return Err(bad("unknown version"));
    }
    let mut count_bytes = [0u8; 8];
    count_bytes.copy_from_slice(&body[8..16]);
    let count = u64::from_le_bytes(count_bytes);
    let mut out = Vec::new();
    let mut pos = 16usize;
    for _ in 0..count {
        let id_end = pos.checked_add(34).filter(|e| *e + 8 <= body.len());
        let Some(id_end) = id_end else {
            return Err(bad("truncated entry"));
        };
        let id = ObjectId::from_bytes(&body[pos..id_end]).map_err(|_| bad("bad object id"))?;
        let mut len_bytes = [0u8; 8];
        len_bytes.copy_from_slice(&body[id_end..id_end + 8]);
        let len = usize::try_from(u64::from_le_bytes(len_bytes)).map_err(|_| bad("bad length"))?;
        let start = id_end + 8;
        let end = start
            .checked_add(len)
            .filter(|e| *e <= body.len())
            .ok_or_else(|| bad("truncated object"))?;
        let bytes = &body[start..end];
        if !verify(&id, bytes) {
            return Err(bad(&format!("object {id} does not match its id")));
        }
        out.push((id, bytes));
        pos = end;
    }
    if pos != body.len() {
        return Err(bad("trailing bytes"));
    }
    Ok(out)
}

/// Fetches, verifies and loads the snapshot of `repo` (scope `sparse`) into the tier's cache.
pub async fn bootstrap(
    tier: &Tier,
    auth: &Auth,
    repo: &str,
    sparse: &[String],
) -> Result<BootstrapReport> {
    let t0 = Instant::now();
    let req = v1::GetDaemonSnapshotRequest {
        repo: Some(repo_ref(repo)),
        sparse_prefixes: sparse.iter().map(|p| p.as_bytes().to_vec()).collect(),
    };
    let up = tier.upstream();
    let snap = with_retries(tier.config().max_retries, || {
        up.daemon_snapshot(auth, req.clone())
    })
    .await?;
    if snap.signed_url.is_empty() {
        return Err(err(
            ErrorReason::Unimplemented,
            "the tier above sent a snapshot without a download URL",
        ));
    }
    let file = up
        .fetch_url(&snap.signed_url)
        .await
        .map_err(|e| unavailable(format!("snapshot download failed: {e}")))?;
    if file.len() as u64 != snap.size_bytes || bee_object::digest(&file) != snap.digest.as_slice() {
        return Err(err(
            ErrorReason::Integrity,
            "snapshot rejected: size or digest differs from the announcement",
        ));
    }
    let objects = decode(&file)?;
    let mut loaded = 0u64;
    for (id, bytes) in &objects {
        if !tier.store().contains(id) {
            tier.store().put(id, bytes)?;
            loaded += 1;
        }
    }
    let changeset = snap
        .root_changeset
        .as_ref()
        .and_then(|i| ObjectId::from_bytes(&i.raw).ok())
        .map(|i| i.to_text())
        .unwrap_or_default();
    Ok(BootstrapReport {
        snapshot: snap.id,
        changeset,
        r#ref: snap.r#ref,
        ref_version: snap.ref_version,
        objects: objects.len() as u64,
        loaded,
        bytes: file.len() as u64,
        millis: t0.elapsed().as_secs_f64() * 1000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(objects: &[&[u8]]) -> Vec<u8> {
        let mut body = MAGIC.to_vec();
        body.extend_from_slice(&VERSION.to_le_bytes());
        body.extend_from_slice(&(objects.len() as u64).to_le_bytes());
        for o in objects {
            body.extend_from_slice(bee_object::blob_id(o).as_bytes());
            body.extend_from_slice(&(o.len() as u64).to_le_bytes());
            body.extend_from_slice(o);
        }
        let t = bee_object::digest(&body);
        body.extend_from_slice(&t);
        body
    }

    #[test]
    fn decodes_and_verifies_every_object() {
        let file = encode(&[b"one", b"two", b""]);
        let got = decode(&file).expect("valid");
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].1, b"two");
    }

    #[test]
    fn rejects_tampering() {
        let mut file = encode(&[b"one", b"two"]);
        let at = 16 + 34 + 8;
        file[at] ^= 1;
        assert!(decode(&file).is_err(), "trailer catches a flipped byte");
        // A consistent trailer over a wrong object still fails the id check.
        let mut body = file[..file.len() - 32].to_vec();
        let t = bee_object::digest(&body);
        body.extend_from_slice(&t);
        let e = decode(&body).expect_err("id mismatch");
        assert!(e.message.contains("does not match its id"), "{}", e.message);
        assert!(decode(b"short").is_err());
        let mut wrong = encode(&[b"x"]);
        wrong[0] = b'X';
        assert!(decode(&wrong).is_err());
    }
}
