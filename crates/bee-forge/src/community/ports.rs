//! Ports: what the community module needs from the rest of the system.
//!
//! The forge never reaches into the VCS data path or other domains' tables. The server (or a
//! test) backs these traits:
//!
//! | port | used for | backed by |
//! |---|---|---|
//! | [`ContentVcs`] | wiki and gist repos, reading files of real repos | the VCS writer and history |
//! | [`TagVcs`] | `tags/*` refs for releases and `ListRepoTags` | the VCS ref store |
//! | [`ChangeSource`] | landed change requests between two tags (generated notes) | the changes domain |
//! | [`ActivitySource`] | log, change request, issue and network data for insights | history and the changes and work domains |
//! | [`IssueBridge`] | converting issues to discussions and back | the work domain |
//! | [`DependencySource`] | org dependency insights | `bee-deps` |
//! | [`UrlSigner`] | upload and download URLs for assets, avatars and exports | the blob store |
//! | [`SponsorPayments`] | checkout for sponsorships | a payment provider |
//!
//! Every port has a detached default that fails with `UNAVAILABLE` (or answers empty where an
//! empty answer is correct), so a forge without a backing still serves everything that does
//! not need it. [`MemoryVcs`], [`MemoryChanges`], [`MemoryActivity`] and [`MemoryIssues`] are
//! complete in-memory implementations for tests and `--dev`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bee_proto::v1::{InsightsOrgDependency, ObjectId, Principal, Repo, Sponsorship};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{ForgeError, Result};
use crate::ids::hex;

/// Which kind of repository a [`VcsRepo`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VcsKind {
    /// A regular repo (read-only through this port).
    Repo,
    /// The `<repo>.wiki` sibling of a repo.
    Wiki,
    /// A gist's backing repo.
    Gist,
}

/// A repository addressed through [`ContentVcs`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VcsRepo {
    /// Kind.
    pub kind: VcsKind,
    /// Repo id (parent repo id for a wiki, gist id for a gist).
    pub id: String,
    /// Display name: full name, `<full name>.wiki`, or `gist/<id>`.
    pub name: String,
}

impl VcsRepo {
    /// A regular repo.
    pub fn of_repo(repo: &Repo) -> VcsRepo {
        VcsRepo {
            kind: VcsKind::Repo,
            id: repo.id.clone(),
            name: repo.full_name.clone(),
        }
    }

    /// The wiki sibling of `repo`.
    pub fn wiki_of(repo: &Repo) -> VcsRepo {
        VcsRepo {
            kind: VcsKind::Wiki,
            id: repo.id.clone(),
            name: format!("{}.wiki", repo.full_name),
        }
    }

    /// The backing repo of gist `id`.
    pub fn gist(id: &str) -> VcsRepo {
        VcsRepo {
            kind: VcsKind::Gist,
            id: id.to_string(),
            name: format!("gist/{id}"),
        }
    }
}

/// One file in a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Path from the root, `/`-separated.
    pub path: String,
    /// Content id of the file.
    pub blob: ObjectId,
    /// Size in bytes.
    pub size: u64,
}

/// A file write (`content` set) or removal (`None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Path from the root.
    pub path: String,
    /// New content, or `None` to delete.
    pub content: Option<Vec<u8>>,
}

/// A commit request: one changeset on the repo's main ref.
#[derive(Clone, Debug)]
pub struct CommitRequest {
    /// The head the caller read; a different head is `VERSION_MISMATCH`.
    pub expected_head: Option<ObjectId>,
    /// Writes and removals.
    pub changes: Vec<FileChange>,
    /// Author.
    pub author: Principal,
    /// Commit message.
    pub message: String,
    /// Commit time (from the forge clock).
    pub time_ms: u64,
}

/// A changeset as the port reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    /// Changeset id.
    pub id: ObjectId,
    /// First parent.
    pub parent: Option<ObjectId>,
    /// Author.
    pub author: Principal,
    /// Message.
    pub message: String,
    /// Time.
    pub time_ms: u64,
    /// Paths changed.
    pub paths: Vec<String>,
    /// Lines added.
    pub additions: u32,
    /// Lines removed.
    pub deletions: u32,
}

fn detached(what: &str) -> ForgeError {
    ForgeError::unavailable(format!("no {what} is configured on this forge"))
        .with_subcode("port_unavailable")
}

/// Content repos: the writable side (wiki and gist repos) and the readable side of regular
/// repos. Writes are one changeset on the repo's main ref.
#[async_trait]
pub trait ContentVcs: Send + Sync {
    /// Creates the repo if it does not exist (idempotent).
    async fn init(&self, repo: &VcsRepo) -> Result<()>;
    /// Removes the repo and its history.
    async fn remove(&self, repo: &VcsRepo) -> Result<()>;
    /// Copies `from` with its history to a new repo `to`.
    async fn fork(&self, from: &VcsRepo, to: &VcsRepo) -> Result<()>;
    /// The head changeset of the main ref, `None` for an empty repo.
    async fn head(&self, repo: &VcsRepo) -> Result<Option<ObjectId>>;
    /// Resolves a revision expression (empty = the default ref); `NOT_FOUND` if unknown.
    async fn resolve(&self, repo: &VcsRepo, expr: &str) -> Result<ObjectId>;
    /// Every file at `at`, sorted by path.
    async fn tree(&self, repo: &VcsRepo, at: &ObjectId) -> Result<Vec<FileEntry>>;
    /// The root tree id at `at` (a cache key).
    async fn tree_id(&self, repo: &VcsRepo, at: &ObjectId) -> Result<ObjectId>;
    /// File content by blob id.
    async fn read(&self, repo: &VcsRepo, blob: &ObjectId) -> Result<Vec<u8>>;
    /// Writes one changeset. `VERSION_MISMATCH` if `expected_head` is stale.
    async fn commit(&self, repo: &VcsRepo, req: CommitRequest) -> Result<CommitInfo>;
    /// History from `at`, newest first, optionally only changesets touching `path`.
    async fn log(
        &self,
        repo: &VcsRepo,
        at: &ObjectId,
        path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CommitInfo>>;
    /// Whether `ancestor` is reachable from `of` (or equal).
    async fn is_ancestor(&self, repo: &VcsRepo, ancestor: &ObjectId, of: &ObjectId)
    -> Result<bool>;
}

/// Reads a file at a revision, `None` when the path or revision does not exist.
pub async fn read_file(
    vcs: &dyn ContentVcs,
    repo: &VcsRepo,
    at: &ObjectId,
    path: &str,
) -> Result<Option<(ObjectId, Vec<u8>)>> {
    let tree = vcs.tree(repo, at).await?;
    match tree.into_iter().find(|f| f.path == path) {
        Some(f) => Ok(Some((f.blob.clone(), vcs.read(repo, &f.blob).await?))),
        None => Ok(None),
    }
}

/// Content VCS that is not configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedVcs;

#[async_trait]
impl ContentVcs for DetachedVcs {
    async fn init(&self, _: &VcsRepo) -> Result<()> {
        Err(detached("content VCS"))
    }
    async fn remove(&self, _: &VcsRepo) -> Result<()> {
        Err(detached("content VCS"))
    }
    async fn fork(&self, _: &VcsRepo, _: &VcsRepo) -> Result<()> {
        Err(detached("content VCS"))
    }
    async fn head(&self, _: &VcsRepo) -> Result<Option<ObjectId>> {
        Err(detached("content VCS"))
    }
    async fn resolve(&self, _: &VcsRepo, _: &str) -> Result<ObjectId> {
        Err(detached("content VCS"))
    }
    async fn tree(&self, _: &VcsRepo, _: &ObjectId) -> Result<Vec<FileEntry>> {
        Err(detached("content VCS"))
    }
    async fn tree_id(&self, _: &VcsRepo, _: &ObjectId) -> Result<ObjectId> {
        Err(detached("content VCS"))
    }
    async fn read(&self, _: &VcsRepo, _: &ObjectId) -> Result<Vec<u8>> {
        Err(detached("content VCS"))
    }
    async fn commit(&self, _: &VcsRepo, _: CommitRequest) -> Result<CommitInfo> {
        Err(detached("content VCS"))
    }
    async fn log(
        &self,
        _: &VcsRepo,
        _: &ObjectId,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<CommitInfo>> {
        Err(detached("content VCS"))
    }
    async fn is_ancestor(&self, _: &VcsRepo, _: &ObjectId, _: &ObjectId) -> Result<bool> {
        Err(detached("content VCS"))
    }
}

/// An annotated tag's annotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagAnnotationInfo {
    /// Annotation object id.
    pub id: ObjectId,
    /// Tagger.
    pub tagger: Principal,
    /// Tag time.
    pub time_ms: u64,
    /// Message.
    pub message: String,
    /// Armored signature, if signed.
    pub signature: Vec<u8>,
    /// Whether the signature verified.
    pub verified: bool,
    /// `valid`, `unknown_key`, `bad_signature` or `unsigned`.
    pub reason: String,
}

/// A tag as the port reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagInfo {
    /// Name without `tags/`.
    pub name: String,
    /// Changeset it resolves to.
    pub target: ObjectId,
    /// Ref version (compare-and-swap).
    pub ref_version: u64,
    /// The annotation, for annotated tags.
    pub annotation: Option<TagAnnotationInfo>,
    /// Policy restricts moving or deleting it.
    pub protected: bool,
}

/// What to annotate a new tag with.
#[derive(Clone, Debug)]
pub struct NewAnnotation {
    /// Tagger.
    pub tagger: Principal,
    /// Message (non-empty).
    pub message: String,
    /// Optional armored signature.
    pub signature: Vec<u8>,
    /// Tag time.
    pub time_ms: u64,
}

/// The `tags/*` namespace of a repo.
#[async_trait]
pub trait TagVcs: Send + Sync {
    /// Tags whose name starts with `prefix`, sorted by name.
    async fn list(&self, repo_id: &str, prefix: &str) -> Result<Vec<TagInfo>>;
    /// One tag.
    async fn get(&self, repo_id: &str, name: &str) -> Result<Option<TagInfo>>;
    /// Creates a tag (`ALREADY_EXISTS` if taken).
    async fn create(
        &self,
        repo_id: &str,
        name: &str,
        target: &ObjectId,
        annotation: Option<NewAnnotation>,
    ) -> Result<TagInfo>;
    /// Deletes a tag; `expected_version` 0 = any.
    async fn delete(&self, repo_id: &str, name: &str, expected_version: u64) -> Result<()>;
}

/// Tag store that is not configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedTags;

#[async_trait]
impl TagVcs for DetachedTags {
    async fn list(&self, _: &str, _: &str) -> Result<Vec<TagInfo>> {
        Err(detached("tag store"))
    }
    async fn get(&self, _: &str, _: &str) -> Result<Option<TagInfo>> {
        Err(detached("tag store"))
    }
    async fn create(
        &self,
        _: &str,
        _: &str,
        _: &ObjectId,
        _: Option<NewAnnotation>,
    ) -> Result<TagInfo> {
        Err(detached("tag store"))
    }
    async fn delete(&self, _: &str, _: &str, _: u64) -> Result<()> {
        Err(detached("tag store"))
    }
}

/// A change request landed in a range of history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedChange {
    /// Change request id.
    pub id: String,
    /// Per-repo number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Author.
    pub author: Principal,
    /// Label names.
    pub labels: Vec<String>,
    /// When it landed.
    pub landed_at_ms: u64,
}

/// Landed change requests, for generated release notes.
#[async_trait]
pub trait ChangeSource: Send + Sync {
    /// Change requests landed in the history of `until` but not in the history of `since`,
    /// in landing order (oldest first). `since` of `None` means from the beginning.
    async fn landed_between(
        &self,
        repo: &Repo,
        since: Option<&ObjectId>,
        until: &ObjectId,
    ) -> Result<Vec<LandedChange>>;
}

/// Change source that is not configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedChanges;

#[async_trait]
impl ChangeSource for DetachedChanges {
    async fn landed_between(
        &self,
        _: &Repo,
        _: Option<&ObjectId>,
        _: &ObjectId,
    ) -> Result<Vec<LandedChange>> {
        Err(detached("change request source"))
    }
}

/// One changeset of the log with its provenance.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogEntry {
    /// Changeset id.
    pub changeset: ObjectId,
    /// Parents.
    pub parents: Vec<ObjectId>,
    /// Author principal; empty id for an unclaimed imported identity.
    pub author: Principal,
    /// For unclaimed identities, the commit email.
    pub placeholder_email: String,
    /// Time.
    pub time_ms: u64,
    /// Lines added.
    pub additions: u64,
    /// Lines removed.
    pub deletions: u64,
    /// Files changed.
    pub files_changed: u32,
    /// First line of the message.
    pub summary: String,
    /// Refs pointing at it.
    pub refs: Vec<String>,
    /// Provenance cost: input tokens, output tokens, USD micros.
    pub cost: (u64, u64, u64),
    /// A later changeset reverted this one.
    pub reverted: bool,
}

/// How a change request ended (or not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeOutcome {
    /// Still open.
    Open,
    /// Landed.
    Landed,
    /// Closed without landing.
    Closed,
}

/// A change request's activity for insights.
#[derive(Clone, Debug)]
pub struct ChangeActivity {
    /// Number.
    pub number: u64,
    /// Author.
    pub author: Principal,
    /// Outcome.
    pub outcome: ChangeOutcome,
    /// Opened at.
    pub opened_ms: u64,
    /// Landed or closed at (0 while open).
    pub ended_ms: u64,
    /// Review rounds (revisions pushed after review).
    pub review_rounds: u32,
}

/// An issue's activity for insights.
#[derive(Clone, Debug)]
pub struct IssueActivity {
    /// Number.
    pub number: u64,
    /// Opened at.
    pub opened_ms: u64,
    /// Closed at (0 while open).
    pub closed_ms: u64,
}

/// A changeset in the network graph with the repo it was seen in.
#[derive(Clone, Debug)]
pub struct GraphEntry {
    /// The changeset.
    pub entry: LogEntry,
    /// Repo id it was seen in.
    pub repo_id: String,
    /// Repo full name.
    pub repo_full_name: String,
}

/// History, change request and issue data for insights.
#[async_trait]
pub trait ActivitySource: Send + Sync {
    /// Landed changesets reachable from `ref_name` (empty = default ref) in the window,
    /// newest first, at most `limit`.
    async fn log(
        &self,
        repo: &Repo,
        ref_name: &str,
        since_ms: u64,
        until_ms: u64,
        limit: usize,
    ) -> Result<Vec<LogEntry>>;
    /// Change requests opened or ended in the window.
    async fn change_requests(
        &self,
        repo: &Repo,
        since_ms: u64,
        until_ms: u64,
    ) -> Result<Vec<ChangeActivity>>;
    /// Issues opened or closed in the window.
    async fn issues(&self, repo: &Repo, since_ms: u64, until_ms: u64)
    -> Result<Vec<IssueActivity>>;
    /// Changesets across refs (and forks) for the network graph, newest first.
    async fn graph(
        &self,
        repo: &Repo,
        include_forks: bool,
        refs: &[String],
        since_ms: u64,
        until_ms: u64,
        limit: usize,
    ) -> Result<Vec<GraphEntry>>;
}

/// Activity source that is not configured: no data.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedActivity;

#[async_trait]
impl ActivitySource for DetachedActivity {
    async fn log(&self, _: &Repo, _: &str, _: u64, _: u64, _: usize) -> Result<Vec<LogEntry>> {
        Ok(Vec::new())
    }
    async fn change_requests(&self, _: &Repo, _: u64, _: u64) -> Result<Vec<ChangeActivity>> {
        Ok(Vec::new())
    }
    async fn issues(&self, _: &Repo, _: u64, _: u64) -> Result<Vec<IssueActivity>> {
        Ok(Vec::new())
    }
    async fn graph(
        &self,
        _: &Repo,
        _: bool,
        _: &[String],
        _: u64,
        _: u64,
        _: usize,
    ) -> Result<Vec<GraphEntry>> {
        Ok(Vec::new())
    }
}

/// An issue as the conversion needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueSnapshot {
    /// Number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Author.
    pub author: Principal,
    /// Label names.
    pub labels: Vec<String>,
}

/// The work domain's side of converting issues and discussions. Besides these calls, every
/// conversion emits `discussion.converted_from_issue` or `discussion.converted_to_issue`
/// events the work domain consumes to close or link the original.
#[async_trait]
pub trait IssueBridge: Send + Sync {
    /// Reads an issue.
    async fn get_issue(&self, repo: &Repo, number: u64) -> Result<IssueSnapshot>;
    /// Opens an issue and returns its number.
    async fn create_issue(
        &self,
        repo: &Repo,
        title: &str,
        body: &str,
        author: &Principal,
        labels: &[String],
    ) -> Result<u64>;

    /// Called after an issue became a discussion: the work domain comments on and closes
    /// the issue. The default does nothing (the `discussion.converted_from_issue` event
    /// carries the same information).
    async fn converted_to_discussion(
        &self,
        _repo: &Repo,
        _number: u64,
        _discussion_url: &str,
        _by: &Principal,
    ) -> Result<()> {
        Ok(())
    }

    /// Whether this is the placeholder that fails every call; the forge then installs the
    /// work-domain bridge.
    fn is_detached(&self) -> bool {
        false
    }
}

/// Issue bridge that is not configured.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedIssues;

#[async_trait]
impl IssueBridge for DetachedIssues {
    fn is_detached(&self) -> bool {
        true
    }

    async fn get_issue(&self, _: &Repo, _: u64) -> Result<IssueSnapshot> {
        Err(detached("issue bridge"))
    }
    async fn create_issue(
        &self,
        _: &Repo,
        _: &str,
        _: &str,
        _: &Principal,
        _: &[String],
    ) -> Result<u64> {
        Err(detached("issue bridge"))
    }
}

/// Filters of an org dependency query.
#[derive(Clone, Debug, Default)]
pub struct DependencyQuery {
    /// Ecosystem, empty = all.
    pub ecosystem: String,
    /// Package name substring.
    pub query: String,
    /// SPDX id filter.
    pub license: String,
    /// Only packages with open advisories.
    pub vulnerable_only: bool,
}

/// Org-wide dependency aggregation (the dependency insights hook).
#[async_trait]
pub trait DependencySource: Send + Sync {
    /// Packages used across the org's repos, sorted by name.
    async fn org_dependencies(
        &self,
        org_id: &str,
        q: &DependencyQuery,
    ) -> Result<Vec<InsightsOrgDependency>>;
}

/// Dependency source that is not configured: no packages.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetachedDependencies;

#[async_trait]
impl DependencySource for DetachedDependencies {
    async fn org_dependencies(
        &self,
        _: &str,
        _: &DependencyQuery,
    ) -> Result<Vec<InsightsOrgDependency>> {
        Ok(Vec::new())
    }
}

/// A signed URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedUrl {
    /// The URL.
    pub url: String,
    /// Headers the request must carry.
    pub headers: Vec<(String, String)>,
    /// Expiry.
    pub expires_ms: u64,
}

/// Signs URLs into the blob store.
pub trait UrlSigner: Send + Sync {
    /// A URL to PUT `size` bytes of `content_type` to `key`.
    fn upload(&self, key: &str, size: u64, content_type: &str, now_ms: u64) -> SignedUrl;
    /// A URL to GET `key`.
    fn download(&self, key: &str, now_ms: u64) -> SignedUrl;
    /// A stable public URL for `key` (avatars).
    fn public(&self, key: &str) -> String;
}

type HmacSha256 = Hmac<Sha256>;

/// Signs URLs with HMAC-SHA256 over method, key and expiry, valid for 15 minutes.
#[derive(Clone, Debug)]
pub struct HmacSigner {
    base: String,
    secret: Vec<u8>,
    ttl_ms: u64,
}

impl HmacSigner {
    /// A signer for `base` (no trailing slash) with `secret`.
    pub fn new(base: &str, secret: &[u8]) -> HmacSigner {
        HmacSigner {
            base: base.trim_end_matches('/').to_string(),
            secret: secret.to_vec(),
            ttl_ms: 15 * 60 * 1000,
        }
    }

    /// A signer with a random secret.
    pub fn random(base: &str) -> HmacSigner {
        let mut secret = [0u8; 32];
        let _ = getrandom::fill(&mut secret);
        HmacSigner::new(base, &secret)
    }

    /// The hex signature of `method`, `key` and `expires_ms`.
    pub fn signature(&self, method: &str, key: &str, expires_ms: u64) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("hmac accepts any key");
        mac.update(format!("{method}\n{key}\n{expires_ms}").as_bytes());
        hex(&mac.finalize().into_bytes())
    }

    /// Whether `sig` is valid for the request and has not expired at `now_ms`.
    pub fn verify(&self, method: &str, key: &str, expires_ms: u64, sig: &str, now_ms: u64) -> bool {
        now_ms < expires_ms && self.signature(method, key, expires_ms) == sig
    }

    fn url(&self, method: &str, key: &str, now_ms: u64) -> (String, u64) {
        let exp = now_ms + self.ttl_ms;
        (
            format!(
                "{}/{key}?expires={exp}&signature={}",
                self.base,
                self.signature(method, key, exp)
            ),
            exp,
        )
    }
}

impl UrlSigner for HmacSigner {
    fn upload(&self, key: &str, size: u64, content_type: &str, now_ms: u64) -> SignedUrl {
        let (url, expires_ms) = self.url("PUT", key, now_ms);
        SignedUrl {
            url,
            headers: vec![
                ("content-type".into(), content_type.to_string()),
                ("content-length".into(), size.to_string()),
            ],
            expires_ms,
        }
    }

    fn download(&self, key: &str, now_ms: u64) -> SignedUrl {
        let (url, expires_ms) = self.url("GET", key, now_ms);
        SignedUrl {
            url,
            headers: Vec::new(),
            expires_ms,
        }
    }

    fn public(&self, key: &str) -> String {
        format!("{}/{key}", self.base)
    }
}

/// Payments for sponsorships.
#[async_trait]
pub trait SponsorPayments: Send + Sync {
    /// Starts payment; `Some(url)` leaves the sponsorship `PENDING` until
    /// [`crate::community::sponsors::Sponsors::confirm_payment`], `None` activates it at once
    /// (offline or invoiced sponsorships).
    async fn checkout(&self, s: &Sponsorship) -> Result<Option<String>>;
    /// Stops future charges.
    async fn cancel(&self, s: &Sponsorship) -> Result<()>;
}

/// Payments that are not configured: sponsorships are recorded and active at once.
#[derive(Clone, Copy, Debug, Default)]
pub struct OfflinePayments;

#[async_trait]
impl SponsorPayments for OfflinePayments {
    async fn checkout(&self, _: &Sponsorship) -> Result<Option<String>> {
        Ok(None)
    }
    async fn cancel(&self, _: &Sponsorship) -> Result<()> {
        Ok(())
    }
}

/// Every port the community module uses.
#[derive(Clone)]
pub struct Ports {
    /// Wiki and gist repos, regular repo files.
    pub vcs: Arc<dyn ContentVcs>,
    /// Tags.
    pub tags: Arc<dyn TagVcs>,
    /// Landed change requests.
    pub changes: Arc<dyn ChangeSource>,
    /// Insights data.
    pub activity: Arc<dyn ActivitySource>,
    /// Issue conversion.
    pub issues: Arc<dyn IssueBridge>,
    /// Org dependencies.
    pub deps: Arc<dyn DependencySource>,
    /// Blob URLs.
    pub signer: Arc<dyn UrlSigner>,
    /// Payments.
    pub payments: Arc<dyn SponsorPayments>,
}

impl Default for Ports {
    fn default() -> Ports {
        Ports {
            vcs: Arc::new(DetachedVcs),
            tags: Arc::new(DetachedTags),
            changes: Arc::new(DetachedChanges),
            activity: Arc::new(DetachedActivity),
            issues: Arc::new(DetachedIssues),
            deps: Arc::new(DetachedDependencies),
            signer: Arc::new(HmacSigner::random("https://blobs.beehive.invalid")),
            payments: Arc::new(OfflinePayments),
        }
    }
}

#[derive(Clone, Debug)]
struct MemCommit {
    info: CommitInfo,
    files: BTreeMap<String, Vec<u8>>,
}

#[derive(Default)]
struct MemRepo {
    commits: BTreeMap<Vec<u8>, MemCommit>,
    refs: BTreeMap<String, Vec<u8>>,
    blobs: HashMap<Vec<u8>, Vec<u8>>,
    tags: BTreeMap<String, (TagInfo, u64)>,
}

/// An in-memory [`ContentVcs`] and [`TagVcs`] for tests and `--dev`.
///
/// Repos are created by [`ContentVcs::init`] (or on the first commit). The main ref is `main`;
/// [`MemoryVcs::set_ref`] adds other refs. Changeset ids are BLAKE3 hashes of parent, files,
/// author, message and time, so histories are deterministic.
#[derive(Default)]
pub struct MemoryVcs {
    repos: Mutex<BTreeMap<(VcsKind, String), MemRepo>>,
}

fn oid(raw: &[u8]) -> ObjectId {
    ObjectId { raw: raw.to_vec() }
}

fn lines(b: &[u8]) -> BTreeSet<&[u8]> {
    b.split(|c| *c == b'\n').collect()
}

impl MemoryVcs {
    /// An empty store.
    pub fn new() -> MemoryVcs {
        MemoryVcs::default()
    }

    fn with<T>(&self, repo: &VcsRepo, f: impl FnOnce(&mut MemRepo) -> Result<T>) -> Result<T> {
        let mut g = self
            .repos
            .lock()
            .map_err(|_| ForgeError::internal("memory vcs poisoned"))?;
        f(g.entry((repo.kind, repo.id.clone())).or_default())
    }

    /// Points `name` at `target` (creating the ref).
    pub fn set_ref(&self, repo: &VcsRepo, name: &str, target: &ObjectId) -> Result<()> {
        self.with(repo, |r| {
            r.refs.insert(name.to_string(), target.raw.clone());
            Ok(())
        })
    }

    /// Commits `files` (path, content) on the main ref as `author` at `time_ms`.
    pub async fn seed(
        &self,
        repo: &VcsRepo,
        files: &[(&str, &str)],
        author: &Principal,
        message: &str,
        time_ms: u64,
    ) -> Result<ObjectId> {
        let head = self.head(repo).await?;
        let c = self
            .commit(
                repo,
                CommitRequest {
                    expected_head: head,
                    changes: files
                        .iter()
                        .map(|(p, c)| FileChange {
                            path: (*p).to_string(),
                            content: Some(c.as_bytes().to_vec()),
                        })
                        .collect(),
                    author: author.clone(),
                    message: message.to_string(),
                    time_ms,
                },
            )
            .await?;
        Ok(c.id)
    }

    fn walk(r: &MemRepo, at: &ObjectId) -> Result<Vec<MemCommit>> {
        let mut out = Vec::new();
        let mut cur = Some(at.raw.clone());
        while let Some(id) = cur {
            let c = r
                .commits
                .get(&id)
                .ok_or_else(|| ForgeError::not_found(format!("changeset {}", hex(&id))))?;
            cur = c.info.parent.as_ref().map(|p| p.raw.clone());
            out.push(c.clone());
        }
        Ok(out)
    }
}

#[async_trait]
impl ContentVcs for MemoryVcs {
    async fn init(&self, repo: &VcsRepo) -> Result<()> {
        self.with(repo, |_| Ok(()))
    }

    async fn remove(&self, repo: &VcsRepo) -> Result<()> {
        let mut g = self
            .repos
            .lock()
            .map_err(|_| ForgeError::internal("memory vcs poisoned"))?;
        g.remove(&(repo.kind, repo.id.clone()));
        Ok(())
    }

    async fn fork(&self, from: &VcsRepo, to: &VcsRepo) -> Result<()> {
        let mut g = self
            .repos
            .lock()
            .map_err(|_| ForgeError::internal("memory vcs poisoned"))?;
        let src = g
            .get(&(from.kind, from.id.clone()))
            .ok_or_else(|| ForgeError::not_found(format!("repo {}", from.name)))?;
        let copy = MemRepo {
            commits: src.commits.clone(),
            refs: src.refs.clone(),
            blobs: src.blobs.clone(),
            tags: BTreeMap::new(),
        };
        g.insert((to.kind, to.id.clone()), copy);
        Ok(())
    }

    async fn head(&self, repo: &VcsRepo) -> Result<Option<ObjectId>> {
        self.with(repo, |r| Ok(r.refs.get("main").map(|h| oid(h))))
    }

    async fn resolve(&self, repo: &VcsRepo, expr: &str) -> Result<ObjectId> {
        self.with(repo, |r| {
            let expr = if expr.is_empty() { "main" } else { expr };
            if let Some(id) = r.refs.get(expr) {
                return Ok(oid(id));
            }
            if let Some((t, _)) = r.tags.get(expr) {
                return Ok(t.target.clone());
            }
            if expr.len() >= 4 && expr.bytes().all(|b| b.is_ascii_hexdigit()) {
                let hits: Vec<&Vec<u8>> = r
                    .commits
                    .keys()
                    .filter(|k| hex(k).starts_with(&expr.to_ascii_lowercase()))
                    .collect();
                if hits.len() == 1 {
                    return Ok(oid(hits[0]));
                }
            }
            Err(ForgeError::not_found(format!("revision {expr}")))
        })
    }

    async fn tree(&self, repo: &VcsRepo, at: &ObjectId) -> Result<Vec<FileEntry>> {
        self.with(repo, |r| {
            let c = r
                .commits
                .get(&at.raw)
                .ok_or_else(|| ForgeError::not_found("changeset"))?;
            Ok(c.files
                .iter()
                .map(|(p, content)| FileEntry {
                    path: p.clone(),
                    blob: oid(blake3::hash(content).as_bytes()),
                    size: content.len() as u64,
                })
                .collect())
        })
    }

    async fn tree_id(&self, repo: &VcsRepo, at: &ObjectId) -> Result<ObjectId> {
        let tree = self.tree(repo, at).await?;
        let mut h = blake3::Hasher::new();
        for f in tree {
            h.update(f.path.as_bytes());
            h.update(&f.blob.raw);
        }
        Ok(oid(h.finalize().as_bytes()))
    }

    async fn read(&self, repo: &VcsRepo, blob: &ObjectId) -> Result<Vec<u8>> {
        self.with(repo, |r| {
            r.blobs
                .get(&blob.raw)
                .cloned()
                .ok_or_else(|| ForgeError::not_found("blob"))
        })
    }

    async fn commit(&self, repo: &VcsRepo, req: CommitRequest) -> Result<CommitInfo> {
        self.with(repo, |r| {
            let head = r.refs.get("main").cloned();
            let want = req.expected_head.as_ref().map(|h| h.raw.clone());
            if want.is_some() && want != head {
                return Err(ForgeError::version_mismatch(1, 2).with_subcode("stale_head"));
            }
            let mut files = head
                .as_ref()
                .and_then(|h| r.commits.get(h))
                .map(|c| c.files.clone())
                .unwrap_or_default();
            let (mut adds, mut dels) = (0u32, 0u32);
            let mut paths = BTreeSet::new();
            for ch in &req.changes {
                let old = files.get(&ch.path).cloned();
                match &ch.content {
                    Some(new) => {
                        let o = old.as_deref().map(lines).unwrap_or_default();
                        let n = lines(new);
                        adds += n.difference(&o).count() as u32;
                        dels += o.difference(&n).count() as u32;
                        r.blobs
                            .insert(blake3::hash(new).as_bytes().to_vec(), new.clone());
                        files.insert(ch.path.clone(), new.clone());
                    }
                    None => {
                        if let Some(o) = old {
                            dels += lines(&o).len() as u32;
                            files.remove(&ch.path);
                        }
                    }
                }
                paths.insert(ch.path.clone());
            }
            let mut h = blake3::Hasher::new();
            h.update(head.as_deref().unwrap_or(&[]));
            for (p, c) in &files {
                h.update(p.as_bytes());
                h.update(blake3::hash(c).as_bytes());
            }
            h.update(req.author.id.as_bytes());
            h.update(req.message.as_bytes());
            h.update(&req.time_ms.to_be_bytes());
            let id = h.finalize().as_bytes().to_vec();
            let info = CommitInfo {
                id: oid(&id),
                parent: head.as_ref().map(|h| oid(h)),
                author: req.author,
                message: req.message,
                time_ms: req.time_ms,
                paths: paths.into_iter().collect(),
                additions: adds,
                deletions: dels,
            };
            r.commits.insert(
                id.clone(),
                MemCommit {
                    info: info.clone(),
                    files,
                },
            );
            r.refs.insert("main".into(), id);
            Ok(info)
        })
    }

    async fn log(
        &self,
        repo: &VcsRepo,
        at: &ObjectId,
        path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CommitInfo>> {
        self.with(repo, |r| {
            Ok(Self::walk(r, at)?
                .into_iter()
                .map(|c| c.info)
                .filter(|i| path.is_none_or(|p| i.paths.iter().any(|x| x == p)))
                .take(limit)
                .collect())
        })
    }

    async fn is_ancestor(
        &self,
        repo: &VcsRepo,
        ancestor: &ObjectId,
        of: &ObjectId,
    ) -> Result<bool> {
        self.with(repo, |r| {
            Ok(Self::walk(r, of)?
                .iter()
                .any(|c| c.info.id.raw == ancestor.raw))
        })
    }
}

#[async_trait]
impl TagVcs for MemoryVcs {
    async fn list(&self, repo_id: &str, prefix: &str) -> Result<Vec<TagInfo>> {
        let vr = VcsRepo {
            kind: VcsKind::Repo,
            id: repo_id.into(),
            name: String::new(),
        };
        self.with(&vr, |r| {
            Ok(r.tags
                .values()
                .filter(|(t, _)| t.name.starts_with(prefix))
                .map(|(t, _)| t.clone())
                .collect())
        })
    }

    async fn get(&self, repo_id: &str, name: &str) -> Result<Option<TagInfo>> {
        Ok(TagVcs::list(self, repo_id, name)
            .await?
            .into_iter()
            .find(|t| t.name == name))
    }

    async fn create(
        &self,
        repo_id: &str,
        name: &str,
        target: &ObjectId,
        annotation: Option<NewAnnotation>,
    ) -> Result<TagInfo> {
        let vr = VcsRepo {
            kind: VcsKind::Repo,
            id: repo_id.into(),
            name: String::new(),
        };
        self.with(&vr, |r| {
            if r.tags.contains_key(name) {
                return Err(ForgeError::already_exists(format!("tag {name}")));
            }
            let annotation = annotation.map(|a| TagAnnotationInfo {
                id: oid(
                    blake3::hash(format!("{name}{}{}", a.message, a.time_ms).as_bytes()).as_bytes(),
                ),
                tagger: a.tagger,
                time_ms: a.time_ms,
                message: a.message,
                reason: if a.signature.is_empty() {
                    "unsigned".into()
                } else {
                    "unknown_key".into()
                },
                signature: a.signature,
                verified: false,
            });
            let t = TagInfo {
                name: name.to_string(),
                target: target.clone(),
                ref_version: 1,
                annotation,
                protected: false,
            };
            r.tags.insert(name.to_string(), (t.clone(), 1));
            Ok(t)
        })
    }

    async fn delete(&self, repo_id: &str, name: &str, expected_version: u64) -> Result<()> {
        let vr = VcsRepo {
            kind: VcsKind::Repo,
            id: repo_id.into(),
            name: String::new(),
        };
        self.with(&vr, |r| match r.tags.get(name) {
            None => Err(ForgeError::not_found(format!("tag {name}"))),
            Some((t, _)) if expected_version != 0 && t.ref_version != expected_version => Err(
                ForgeError::version_mismatch(expected_version, t.ref_version),
            ),
            Some(_) => {
                r.tags.remove(name);
                Ok(())
            }
        })
    }
}

/// An in-memory [`ChangeSource`] over a [`MemoryVcs`]: each landed change is recorded with the
/// changeset the repo's ref pointed at after it landed, and membership in a range is decided by
/// ancestry in the VCS.
pub struct MemoryChanges {
    vcs: Arc<MemoryVcs>,
    landed: Mutex<Vec<(String, ObjectId, LandedChange)>>,
}

impl MemoryChanges {
    /// A source over `vcs`.
    pub fn new(vcs: Arc<MemoryVcs>) -> MemoryChanges {
        MemoryChanges {
            vcs,
            landed: Mutex::new(Vec::new()),
        }
    }

    /// Records a change request that landed at `at` in repo `repo_id`.
    pub fn land(&self, repo_id: &str, at: &ObjectId, change: LandedChange) {
        if let Ok(mut l) = self.landed.lock() {
            l.push((repo_id.to_string(), at.clone(), change));
        }
    }
}

#[async_trait]
impl ChangeSource for MemoryChanges {
    async fn landed_between(
        &self,
        repo: &Repo,
        since: Option<&ObjectId>,
        until: &ObjectId,
    ) -> Result<Vec<LandedChange>> {
        let vr = VcsRepo::of_repo(repo);
        let all: Vec<(ObjectId, LandedChange)> = self
            .landed
            .lock()
            .map_err(|_| ForgeError::internal("memory changes poisoned"))?
            .iter()
            .filter(|(r, _, _)| *r == repo.id)
            .map(|(_, at, c)| (at.clone(), c.clone()))
            .collect();
        let mut out = Vec::new();
        for (at, c) in all {
            if !self.vcs.is_ancestor(&vr, &at, until).await? {
                continue;
            }
            if let Some(s) = since
                && self.vcs.is_ancestor(&vr, &at, s).await?
            {
                continue;
            }
            out.push(c);
        }
        out.sort_by_key(|c| (c.landed_at_ms, c.number));
        Ok(out)
    }
}

/// An in-memory [`ActivitySource`] filled by the caller.
#[derive(Default)]
pub struct MemoryActivity {
    data: Mutex<ActivityData>,
}

#[derive(Default)]
struct ActivityData {
    log: HashMap<String, Vec<LogEntry>>,
    crs: HashMap<String, Vec<ChangeActivity>>,
    issues: HashMap<String, Vec<IssueActivity>>,
    forks: HashMap<String, Vec<(String, String, Vec<LogEntry>)>>,
}

impl MemoryActivity {
    /// An empty source.
    pub fn new() -> MemoryActivity {
        MemoryActivity::default()
    }

    /// Sets the log of a repo.
    pub fn set_log(&self, repo_id: &str, log: Vec<LogEntry>) {
        if let Ok(mut d) = self.data.lock() {
            d.log.insert(repo_id.into(), log);
        }
    }

    /// Sets the change request activity of a repo.
    pub fn set_change_requests(&self, repo_id: &str, crs: Vec<ChangeActivity>) {
        if let Ok(mut d) = self.data.lock() {
            d.crs.insert(repo_id.into(), crs);
        }
    }

    /// Sets the issue activity of a repo.
    pub fn set_issues(&self, repo_id: &str, issues: Vec<IssueActivity>) {
        if let Ok(mut d) = self.data.lock() {
            d.issues.insert(repo_id.into(), issues);
        }
    }

    /// Adds a fork's log to the network graph of `repo_id`.
    pub fn add_fork(&self, repo_id: &str, fork_id: &str, fork_name: &str, log: Vec<LogEntry>) {
        if let Ok(mut d) = self.data.lock() {
            d.forks.entry(repo_id.into()).or_default().push((
                fork_id.into(),
                fork_name.into(),
                log,
            ));
        }
    }
}

fn in_window(t: u64, since: u64, until: u64) -> bool {
    t >= since && (until == 0 || t < until)
}

#[async_trait]
impl ActivitySource for MemoryActivity {
    async fn log(
        &self,
        repo: &Repo,
        _ref_name: &str,
        since_ms: u64,
        until_ms: u64,
        limit: usize,
    ) -> Result<Vec<LogEntry>> {
        let d = self
            .data
            .lock()
            .map_err(|_| ForgeError::internal("memory activity poisoned"))?;
        let mut v: Vec<LogEntry> = d
            .log
            .get(&repo.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| in_window(e.time_ms, since_ms, until_ms))
            .collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.time_ms));
        v.truncate(limit);
        Ok(v)
    }

    async fn change_requests(
        &self,
        repo: &Repo,
        since_ms: u64,
        until_ms: u64,
    ) -> Result<Vec<ChangeActivity>> {
        let d = self
            .data
            .lock()
            .map_err(|_| ForgeError::internal("memory activity poisoned"))?;
        Ok(d.crs
            .get(&repo.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|c| {
                in_window(c.opened_ms, since_ms, until_ms)
                    || (c.ended_ms > 0 && in_window(c.ended_ms, since_ms, until_ms))
            })
            .collect())
    }

    async fn issues(
        &self,
        repo: &Repo,
        since_ms: u64,
        until_ms: u64,
    ) -> Result<Vec<IssueActivity>> {
        let d = self
            .data
            .lock()
            .map_err(|_| ForgeError::internal("memory activity poisoned"))?;
        Ok(d.issues
            .get(&repo.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| {
                in_window(i.opened_ms, since_ms, until_ms)
                    || (i.closed_ms > 0 && in_window(i.closed_ms, since_ms, until_ms))
            })
            .collect())
    }

    async fn graph(
        &self,
        repo: &Repo,
        include_forks: bool,
        refs: &[String],
        since_ms: u64,
        until_ms: u64,
        limit: usize,
    ) -> Result<Vec<GraphEntry>> {
        let d = self
            .data
            .lock()
            .map_err(|_| ForgeError::internal("memory activity poisoned"))?;
        let mut out: Vec<GraphEntry> = Vec::new();
        let mut add = |id: &str, name: &str, log: &[LogEntry]| {
            for e in log {
                if !in_window(e.time_ms, since_ms, until_ms) {
                    continue;
                }
                if !refs.is_empty() && !e.refs.iter().any(|r| refs.contains(r)) {
                    continue;
                }
                out.push(GraphEntry {
                    entry: e.clone(),
                    repo_id: id.to_string(),
                    repo_full_name: name.to_string(),
                });
            }
        };
        add(
            &repo.id,
            &repo.full_name,
            d.log.get(&repo.id).map(Vec::as_slice).unwrap_or(&[]),
        );
        if include_forks {
            for (id, name, log) in d.forks.get(&repo.id).into_iter().flatten() {
                add(id, name, log);
            }
        }
        out.sort_by_key(|g| std::cmp::Reverse(g.entry.time_ms));
        out.truncate(limit);
        Ok(out)
    }
}

/// An in-memory [`IssueBridge`].
#[derive(Default)]
pub struct MemoryIssues {
    issues: Mutex<HashMap<(String, u64), IssueSnapshot>>,
}

impl MemoryIssues {
    /// An empty bridge.
    pub fn new() -> MemoryIssues {
        MemoryIssues::default()
    }

    /// Adds an issue.
    pub fn add(&self, repo_id: &str, issue: IssueSnapshot) {
        if let Ok(mut m) = self.issues.lock() {
            m.insert((repo_id.to_string(), issue.number), issue);
        }
    }

    /// A stored issue.
    pub fn get(&self, repo_id: &str, number: u64) -> Option<IssueSnapshot> {
        self.issues
            .lock()
            .ok()?
            .get(&(repo_id.to_string(), number))
            .cloned()
    }
}

#[async_trait]
impl IssueBridge for MemoryIssues {
    async fn get_issue(&self, repo: &Repo, number: u64) -> Result<IssueSnapshot> {
        self.get(&repo.id, number)
            .ok_or_else(|| ForgeError::not_found(format!("issue #{number}")))
    }

    async fn create_issue(
        &self,
        repo: &Repo,
        title: &str,
        body: &str,
        author: &Principal,
        labels: &[String],
    ) -> Result<u64> {
        let mut m = self
            .issues
            .lock()
            .map_err(|_| ForgeError::internal("memory issues poisoned"))?;
        let number = m
            .keys()
            .filter(|(r, _)| *r == repo.id)
            .map(|(_, n)| *n)
            .max()
            .unwrap_or(0)
            + 1;
        m.insert(
            (repo.id.clone(), number),
            IssueSnapshot {
                number,
                title: title.into(),
                body: body.into(),
                author: author.clone(),
                labels: labels.to_vec(),
            },
        );
        Ok(number)
    }
}
