//! What the security module needs from the rest of the system. The server backs each port
//! (`docs/spec/security.md` §13); the defaults here make the module usable without them and
//! the in-memory implementations drive the tests.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bee_deps::update::PlannedUpdate;
use bee_deps::{Ecosystem, SourceFile};
use bee_proto::v1::{ObjectId, Repo, SecretScanningValidity};
use sha2::{Digest, Sha256};

use crate::error::{ForgeError, Result};
use crate::service::unserved;

/// A file of a revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    /// Repo-relative path.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
}

/// A blob a changeset adds or changes relative to its first parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddedBlob {
    /// Repo-relative path.
    pub path: String,
    /// Blob id.
    pub blob: ObjectId,
    /// Content.
    pub bytes: Vec<u8>,
}

/// Read access to a repo's content: revisions, trees and blobs.
#[async_trait]
pub trait SecurityVcs: Send + Sync {
    /// The changeset `expr` names; an empty expression is the default ref's tip. `NOT_FOUND`
    /// when it does not resolve.
    async fn resolve(&self, repo: &Repo, expr: &str) -> Result<ObjectId>;
    /// Every file (no directories) of the changeset's tree, sorted by path.
    async fn list_files(&self, repo: &Repo, at: &ObjectId) -> Result<Vec<TreeFile>>;
    /// A file's bytes; `None` when absent.
    async fn read_file(&self, repo: &Repo, at: &ObjectId, path: &str) -> Result<Option<Vec<u8>>>;
    /// The blobs `at` adds or changes against its first parent (all files for a root).
    async fn added_blobs(&self, repo: &Repo, at: &ObjectId) -> Result<Vec<AddedBlob>>;
    /// Changesets reachable from `tip`, newest first, at most `limit`.
    async fn history(&self, repo: &Repo, tip: &ObjectId, limit: usize) -> Result<Vec<ObjectId>>;
    /// Paths of the files whose entry differs between the trees of `from` and `to` (added,
    /// changed or removed), at a cost proportional to the difference rather than the tree.
    /// `None` when the port cannot tell cheaply; callers then fall back to
    /// [`SecurityVcs::list_files`].
    async fn changed_paths(
        &self,
        _repo: &Repo,
        _from: &ObjectId,
        _to: &ObjectId,
    ) -> Result<Option<Vec<String>>> {
        Ok(None)
    }
}

/// A VCS port for deployments where content is served elsewhere: every call is
/// `UNIMPLEMENTED` / `served_elsewhere`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedVcs;

#[async_trait]
impl SecurityVcs for DetachedVcs {
    async fn resolve(&self, _: &Repo, _: &str) -> Result<ObjectId> {
        Err(unserved("security content access", "the VCS (bee-server)"))
    }
    async fn list_files(&self, _: &Repo, _: &ObjectId) -> Result<Vec<TreeFile>> {
        Err(unserved("security content access", "the VCS (bee-server)"))
    }
    async fn read_file(&self, _: &Repo, _: &ObjectId, _: &str) -> Result<Option<Vec<u8>>> {
        Err(unserved("security content access", "the VCS (bee-server)"))
    }
    async fn added_blobs(&self, _: &Repo, _: &ObjectId) -> Result<Vec<AddedBlob>> {
        Err(unserved("security content access", "the VCS (bee-server)"))
    }
    async fn history(&self, _: &Repo, _: &ObjectId, _: usize) -> Result<Vec<ObjectId>> {
        Err(unserved("security content access", "the VCS (bee-server)"))
    }
}

/// The source of OSV advisories. Fetching and mirroring happen behind this port; matching
/// never touches the network.
#[async_trait]
pub trait AdvisoryFeed: Send + Sync {
    /// An opaque version of the current contents; an unchanged version means "nothing to
    /// reload".
    async fn version(&self) -> Result<String>;
    /// Every OSV record as JSON text, one record per entry.
    async fn load(&self) -> Result<Vec<String>>;
}

/// An empty feed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAdvisories;

#[async_trait]
impl AdvisoryFeed for NoAdvisories {
    async fn version(&self) -> Result<String> {
        Ok(String::new())
    }
    async fn load(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// An in-memory feed whose records tests replace.
#[derive(Default)]
pub struct MemoryFeed {
    records: RwLock<(u64, Vec<String>)>,
}

impl MemoryFeed {
    /// An empty feed.
    pub fn new() -> MemoryFeed {
        MemoryFeed::default()
    }

    /// Replaces every record and bumps the version.
    pub fn set(&self, records: Vec<String>) {
        if let Ok(mut r) = self.records.write() {
            r.0 += 1;
            r.1 = records;
        }
    }
}

#[async_trait]
impl AdvisoryFeed for MemoryFeed {
    async fn version(&self) -> Result<String> {
        Ok(self
            .records
            .read()
            .map(|r| r.0.to_string())
            .unwrap_or_default())
    }
    async fn load(&self) -> Result<Vec<String>> {
        Ok(self.records.read().map(|r| r.1.clone()).unwrap_or_default())
    }
}

/// A feed over a directory of OSV JSON files (`*.json`, `*.jsonl`, recursively). The version is
/// a digest of the file names, sizes and modification times.
pub struct DirFeed {
    dir: PathBuf,
}

impl DirFeed {
    /// A feed over `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> DirFeed {
        DirFeed { dir: dir.into() }
    }

    fn files(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let mut stack = vec![self.dir.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)? {
                let p = e?.path();
                if p.is_dir() {
                    stack.push(p);
                } else if matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("json" | "jsonl")
                ) {
                    out.push(p);
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

#[async_trait]
impl AdvisoryFeed for DirFeed {
    async fn version(&self) -> Result<String> {
        let files = self
            .files()
            .map_err(|e| ForgeError::unavailable(format!("advisory directory: {e}")))?;
        let mut h = Sha256::new();
        for f in files {
            let md = std::fs::metadata(&f)
                .map_err(|e| ForgeError::unavailable(format!("advisory file: {e}")))?;
            let mtime = md
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            h.update(f.to_string_lossy().as_bytes());
            h.update(md.len().to_be_bytes());
            h.update(mtime.to_be_bytes());
        }
        Ok(crate::ids::hex(&h.finalize()))
    }

    async fn load(&self) -> Result<Vec<String>> {
        let files = self
            .files()
            .map_err(|e| ForgeError::unavailable(format!("advisory directory: {e}")))?;
        let mut out = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f)
                .map_err(|e| ForgeError::unavailable(format!("advisory file: {e}")))?;
            let jsonl = f.extension().and_then(|e| e.to_str()) == Some("jsonl");
            let t = text.trim();
            if jsonl || (!t.starts_with('{') && !t.starts_with('[')) {
                out.extend(
                    text.lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(str::to_string),
                );
            } else if t.starts_with('[') {
                if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(t) {
                    out.extend(items.iter().map(|v| v.to_string()));
                }
            } else {
                out.push(text);
            }
        }
        Ok(out)
    }
}

/// Checks whether a detected secret still works (a partner validity check). The secret value is
/// handed over in memory only for the call and never stored.
#[async_trait]
pub trait SecretVerifier: Send + Sync {
    /// The validity of `secret` of `secret_type` (for example `github_pat`).
    async fn verify(&self, secret_type: &str, secret: &[u8]) -> SecretScanningValidity;
}

/// A verifier that knows nothing: every secret is `UNKNOWN`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoVerifier;

#[async_trait]
impl SecretVerifier for NoVerifier {
    async fn verify(&self, _: &str, _: &[u8]) -> SecretScanningValidity {
        SecretScanningValidity::Unknown
    }
}

/// Registry knowledge the update bot needs: newest versions (scheduled version updates) and the
/// integrity values lockfiles record for a version.
#[async_trait]
pub trait Registry: Send + Sync {
    /// The newest stable version of a package, if known.
    async fn latest_version(&self, ecosystem: Ecosystem, name: &str) -> Option<String>;
    /// The integrity string a lockfile records for a version: npm `sha512-...`, Cargo checksum
    /// hex, Go `h1:...` (a module version, or `<version>/go.mod` for the go.mod hash), pip
    /// `sha256:<hex>` (several joined by a comma).
    async fn integrity(&self, ecosystem: Ecosystem, name: &str, version: &str) -> Option<String>;
}

/// A registry that knows nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRegistry;

#[async_trait]
impl Registry for NoRegistry {
    async fn latest_version(&self, _: Ecosystem, _: &str) -> Option<String> {
        None
    }
    async fn integrity(&self, _: Ecosystem, _: &str, _: &str) -> Option<String> {
        None
    }
}

/// A registry with fixed answers, for tests and offline deployments.
#[derive(Default)]
pub struct MemoryRegistry {
    latest: RwLock<BTreeMap<(Ecosystem, String), String>>,
    integrity: RwLock<BTreeMap<(Ecosystem, String, String), String>>,
}

impl MemoryRegistry {
    /// An empty registry.
    pub fn new() -> MemoryRegistry {
        MemoryRegistry::default()
    }

    /// Sets the newest version of a package.
    pub fn set_latest(&self, ecosystem: Ecosystem, name: &str, version: &str) {
        if let Ok(mut m) = self.latest.write() {
            m.insert((ecosystem, name.to_string()), version.to_string());
        }
    }

    /// Sets the integrity of a version.
    pub fn set_integrity(&self, ecosystem: Ecosystem, name: &str, version: &str, value: &str) {
        if let Ok(mut m) = self.integrity.write() {
            m.insert(
                (ecosystem, name.to_string(), version.to_string()),
                value.to_string(),
            );
        }
    }
}

#[async_trait]
impl Registry for MemoryRegistry {
    async fn latest_version(&self, ecosystem: Ecosystem, name: &str) -> Option<String> {
        self.latest
            .read()
            .ok()?
            .get(&(ecosystem, name.to_string()))
            .cloned()
    }
    async fn integrity(&self, ecosystem: Ecosystem, name: &str, version: &str) -> Option<String> {
        self.integrity
            .read()
            .ok()?
            .get(&(ecosystem, name.to_string(), version.to_string()))
            .cloned()
    }
}

/// Refreshes lockfiles after the manifests of an update were edited. The built-in
/// [`crate::security::lockfile::TextLockRefresher`] rewrites the entries of the updated packages
/// in place; the server can replace it with one that runs the ecosystem's tool in a sandbox.
#[async_trait]
pub trait LockRefresher: Send + Sync {
    /// The files to write for `updates`, given every dependency file of the revision with the
    /// manifest edits already applied. Only changed or new files are returned.
    async fn refresh(
        &self,
        files: &[SourceFile],
        updates: &[PlannedUpdate],
    ) -> Result<Vec<SourceFile>>;
}

/// Fetches the document behind a signed URL (SARIF uploads by reference).
#[async_trait]
pub trait UrlFetcher: Send + Sync {
    /// The body of `url`, at most `limit` bytes.
    async fn fetch(&self, url: &str, limit: usize) -> Result<Vec<u8>>;
}

/// Refuses every URL: deployments without an egress policy accept inline uploads only.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFetch;

#[async_trait]
impl UrlFetcher for NoFetch {
    async fn fetch(&self, _: &str, _: usize) -> Result<Vec<u8>> {
        Err(ForgeError::invalid(
            "url_fetch_unavailable",
            "this deployment does not fetch SARIF by URL; upload the gzip bytes",
        ))
    }
}

/// Publishes large documents (SBOMs) and returns a short-lived signed URL.
#[async_trait]
pub trait DocumentPublisher: Send + Sync {
    /// Stores `bytes` and returns a signed URL and its expiry in milliseconds since the epoch.
    async fn publish(
        &self,
        name: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<(String, u64)>;
}

/// Starts an agent run that fixes a code scanning alert (autofix).
#[async_trait]
pub trait AutofixRunner: Send + Sync {
    /// Starts the run and returns its id.
    async fn start(
        &self,
        repo: &Repo,
        alert_number: u64,
        rule_id: &str,
        agent_runtime_id: &str,
    ) -> Result<String>;
}

/// No agent runtime: autofix is served by the agents module.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAutofix;

#[async_trait]
impl AutofixRunner for NoAutofix {
    async fn start(&self, _: &Repo, _: u64, _: &str, _: &str) -> Result<String> {
        Err(unserved("RequestCodeScanningAutofix", "agents"))
    }
}

/// Verifies signatures of changesets and tags.
#[async_trait]
pub trait SignatureVerifier: Send + Sync {
    /// The verification of a changeset or tag.
    async fn verify(
        &self,
        repo: &Repo,
        changeset: Option<&ObjectId>,
        tag: &str,
    ) -> Result<bee_proto::v1::VerifyObjectSignatureResponse>;
}

/// No signature backend: signature verification is served by the VCS.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSignatures;

#[async_trait]
impl SignatureVerifier for NoSignatures {
    async fn verify(
        &self,
        _: &Repo,
        _: Option<&ObjectId>,
        _: &str,
    ) -> Result<bee_proto::v1::VerifyObjectSignatureResponse> {
        Err(unserved("VerifyObjectSignature", "the VCS (bee-server)"))
    }
}

/// Creates the temporary private fork a repo advisory uses for its fix.
#[async_trait]
pub trait AdvisoryForks: Send + Sync {
    /// A private fork of `repo` for the advisory `ghsa_id`.
    async fn create_private_fork(&self, repo: &Repo, ghsa_id: &str) -> Result<Repo>;
}

/// Every port of the security module.
#[derive(Clone)]
pub struct Ports {
    /// Repo content.
    pub vcs: Arc<dyn SecurityVcs>,
    /// OSV advisories.
    pub feed: Arc<dyn AdvisoryFeed>,
    /// Partner validity checks.
    pub verifier: Arc<dyn SecretVerifier>,
    /// Registry metadata.
    pub registry: Arc<dyn Registry>,
    /// Lockfile refresh; `None` uses [`crate::security::lockfile::TextLockRefresher`].
    pub locks: Option<Arc<dyn LockRefresher>>,
    /// Fetch by URL.
    pub fetcher: Arc<dyn UrlFetcher>,
    /// Signed document URLs; `None` keeps SBOMs inline.
    pub publisher: Option<Arc<dyn DocumentPublisher>>,
    /// Autofix agent runs.
    pub autofix: Arc<dyn AutofixRunner>,
    /// Signature verification.
    pub signatures: Arc<dyn SignatureVerifier>,
    /// Private forks for advisories; `None` answers `served_elsewhere`.
    pub forks: Option<Arc<dyn AdvisoryForks>>,
}

impl Default for Ports {
    fn default() -> Ports {
        Ports {
            vcs: Arc::new(DetachedVcs),
            feed: Arc::new(NoAdvisories),
            verifier: Arc::new(NoVerifier),
            registry: Arc::new(NoRegistry),
            locks: None,
            fetcher: Arc::new(NoFetch),
            publisher: None,
            autofix: Arc::new(NoAutofix),
            signatures: Arc::new(NoSignatures),
            forks: None,
        }
    }
}
