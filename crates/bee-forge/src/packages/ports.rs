//! What the packages module needs from the rest of the system. Every port has a detached
//! default that fails with `UNAVAILABLE` (or answers "no" where that is correct), so a forge
//! without a backing still serves everything that does not need it; [`MemoryBlobs`] is a
//! complete in-memory blob port for tests and `--dev`.
//!
//! | port | backed by |
//! |---|---|
//! | [`BlobPort`] | the blob store (`bee-store-blob`), under a `pkg/` key prefix |
//! | [`crate::community::ports::UrlSigner`] | signed upload and download URLs for the same keys |
//! | [`TeamMembership`] | enterprise teams |
//! | [`ProvenanceVerifier`] | workflow attestations (`WorkflowService.ListAttestations`) |

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;

use crate::community::ports::{HmacSigner, UrlSigner};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};

/// Object storage for package content. Keys are `/`-separated and chosen by the module
/// (`pkg/<owner id>/sha256/<hex>` for content-addressed blobs, `pkg/<owner id>/up/<upload>/...`
/// for native uploads that clients write through signed URLs).
#[async_trait]
pub trait BlobPort: Send + Sync {
    /// Stores `data` under `key` (overwriting).
    async fn put(&self, key: &str, data: Bytes) -> Result<()>;
    /// Reads `key`; `None` when absent.
    async fn get(&self, key: &str) -> Result<Option<Bytes>>;
    /// Size of `key`; `None` when absent.
    async fn head(&self, key: &str) -> Result<Option<u64>>;
    /// Removes `key` (absent is fine).
    async fn delete(&self, key: &str) -> Result<()>;
}

/// A blob port that is not configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedBlobs;

fn detached(what: &str) -> ForgeError {
    ForgeError::unavailable(format!("no {what} is configured on this forge"))
        .with_subcode("port_unavailable")
}

#[async_trait]
impl BlobPort for DetachedBlobs {
    async fn put(&self, _: &str, _: Bytes) -> Result<()> {
        Err(detached("package blob store"))
    }
    async fn get(&self, _: &str) -> Result<Option<Bytes>> {
        Err(detached("package blob store"))
    }
    async fn head(&self, _: &str) -> Result<Option<u64>> {
        Err(detached("package blob store"))
    }
    async fn delete(&self, _: &str) -> Result<()> {
        Err(detached("package blob store"))
    }
}

/// An in-memory blob port.
#[derive(Default)]
pub struct MemoryBlobs {
    map: Mutex<BTreeMap<String, Bytes>>,
}

impl MemoryBlobs {
    /// An empty store.
    pub fn new() -> MemoryBlobs {
        MemoryBlobs::default()
    }

    /// Number of stored objects.
    pub fn len(&self) -> usize {
        self.map.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Whether nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `key` exists.
    pub fn contains(&self, key: &str) -> bool {
        self.map
            .lock()
            .map(|m| m.contains_key(key))
            .unwrap_or(false)
    }

    /// Every key, sorted.
    pub fn keys(&self) -> Vec<String> {
        self.map
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn with<T>(&self, f: impl FnOnce(&mut BTreeMap<String, Bytes>) -> T) -> Result<T> {
        let mut g = self
            .map
            .lock()
            .map_err(|_| ForgeError::internal("memory blob store poisoned"))?;
        Ok(f(&mut g))
    }
}

#[async_trait]
impl BlobPort for MemoryBlobs {
    async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        self.with(|m| {
            m.insert(key.to_string(), data);
        })
    }
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        self.with(|m| m.get(key).cloned())
    }
    async fn head(&self, key: &str) -> Result<Option<u64>> {
        self.with(|m| m.get(key).map(|b| b.len() as u64))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.with(|m| {
            m.remove(key);
        })
    }
}

/// Answers whether a principal belongs to a team of an org (for team grants on packages).
#[async_trait]
pub trait TeamMembership: Send + Sync {
    /// Whether `principal_id` is in team `team_id` of org `org_id`.
    async fn is_member(&self, org_id: &str, team_id: &str, principal_id: &str) -> bool;
}

/// No teams: a team grant matches nobody.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedTeams;

#[async_trait]
impl TeamMembership for DetachedTeams {
    async fn is_member(&self, _: &str, _: &str, _: &str) -> bool {
        false
    }
}

/// What a publisher claims about how a version was built.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProvenanceClaim {
    /// Source repo id.
    pub repo_id: String,
    /// Source ref.
    pub git_ref: String,
    /// Source changeset id (raw bytes).
    pub changeset: Vec<u8>,
    /// Workflow run id.
    pub run_id: String,
    /// Workflow file path.
    pub workflow_path: String,
    /// Job id.
    pub job_id: String,
    /// Certificate identity of the signer.
    pub signer_identity: String,
    /// OIDC issuer of the signer.
    pub issuer: String,
    /// Attestation ids.
    pub attestation_ids: Vec<String>,
}

/// Checks a provenance claim against workflow records and attestations.
#[async_trait]
pub trait ProvenanceVerifier: Send + Sync {
    /// Whether the claim is backed by the workflow system (a real run of that repo, with the
    /// named attestations). Claims that fail still get recorded, as unverified.
    async fn verify(&self, cx: &Cx, claim: &ProvenanceClaim) -> Result<bool>;
}

/// Verifies nothing: every claim is recorded as unverified.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unverified;

#[async_trait]
impl ProvenanceVerifier for Unverified {
    async fn verify(&self, _: &Cx, _: &ProvenanceClaim) -> Result<bool> {
        Ok(false)
    }
}

/// The ports of the packages module.
#[derive(Clone)]
pub struct PackagePorts {
    /// Package content.
    pub blobs: Arc<dyn BlobPort>,
    /// Signed URLs into the same store, for native publish and downloads.
    pub signer: Arc<dyn UrlSigner>,
    /// Team membership for team grants.
    pub teams: Arc<dyn TeamMembership>,
    /// Provenance verification.
    pub provenance: Arc<dyn ProvenanceVerifier>,
}

impl Default for PackagePorts {
    fn default() -> PackagePorts {
        PackagePorts {
            blobs: Arc::new(DetachedBlobs),
            signer: Arc::new(HmacSigner::random("https://blobs.beehive.invalid")),
            teams: Arc::new(DetachedTeams),
            provenance: Arc::new(Unverified),
        }
    }
}

impl PackagePorts {
    /// Ports over an in-memory blob store (tests and `--dev`).
    pub fn in_memory() -> (PackagePorts, Arc<MemoryBlobs>) {
        let blobs = Arc::new(MemoryBlobs::new());
        (
            PackagePorts {
                blobs: blobs.clone(),
                ..PackagePorts::default()
            },
            blobs,
        )
    }
}
