//! What the gateway needs from outside the forge. The server wires real implementations;
//! [`crate::memory`] has in-process ones over the bee stores for tests and `--dev`.
//!
//! | port | answers |
//! |---|---|
//! | [`GitPort`] | refs, commits, trees, blobs, paths, comparisons and git-data writes, all in git ids |
//! | [`TokenMap`] | a presented GitHub-style token → the bee token the forge verifies |
//! | [`CompatState`] | small documents only the gateway keeps (a PR's head branch name) |
//! | [`AssetStore`] | release asset bytes, put at the signed URL the forge issued |
//! | [`CodeSearch`] | `GET /search/code` |
//! | [`RefMover`] | ref moves of the git data and contents APIs through the server's write path |

use async_trait::async_trait;
use bee_proto::v1::{ObjectId, Repo};
use bytes::Bytes;

use crate::error::ApiResult;

/// A git identity line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Signature {
    /// Name.
    pub name: String,
    /// Email (may be empty).
    pub email: String,
    /// Seconds since the epoch.
    pub time_secs: i64,
}

/// A git commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitCommitInfo {
    /// Commit id (hex).
    pub sha: String,
    /// Tree id (hex).
    pub tree: String,
    /// Parent ids (hex), in order.
    pub parents: Vec<String>,
    /// Full message.
    pub message: String,
    /// Author.
    pub author: Signature,
    /// Committer.
    pub committer: Signature,
    /// The commit's signature, when it carries one (GitHub's `verification`).
    pub verification: Option<CommitVerification>,
}

/// The signature of a commit and whether it verified (GitHub's `verification` object).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitVerification {
    /// Whether it verified under a key registered to an account.
    pub verified: bool,
    /// GitHub's reason (`valid`, `unknown_key`, `invalid`, `malformed_signature`, ...).
    pub reason: String,
    /// The armored signature.
    pub signature: String,
    /// The signed payload (the commit without its signature header).
    pub payload: String,
}

/// A git ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRefInfo {
    /// Full git name (`refs/heads/main`, `refs/tags/v1`).
    pub name: String,
    /// Target commit id (hex).
    pub sha: String,
}

/// The kind of a tree entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A file or symlink.
    Blob,
    /// A directory.
    Tree,
    /// A submodule.
    Commit,
}

impl EntryKind {
    /// GitHub's `type`.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Blob => "blob",
            EntryKind::Tree => "tree",
            EntryKind::Commit => "commit",
        }
    }
}

/// One entry of a git tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeItem {
    /// Path relative to the listed tree (`a/b.txt` when recursive).
    pub path: String,
    /// Git mode (`100644`, `100755`, `120000`, `040000`, `160000`).
    pub mode: String,
    /// Kind.
    pub kind: EntryKind,
    /// Object id (hex).
    pub sha: String,
    /// Blob size.
    pub size: Option<u64>,
}

/// A git tree listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeListing {
    /// Tree id (hex).
    pub sha: String,
    /// Entries in git order.
    pub entries: Vec<TreeItem>,
    /// Whether a recursive listing stopped at the limit.
    pub truncated: bool,
}

/// What a path names at a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathEntry {
    /// A regular or executable file.
    File {
        /// Blob id.
        sha: String,
        /// Bytes.
        bytes: Vec<u8>,
    },
    /// A symlink.
    Symlink {
        /// Blob id.
        sha: String,
        /// Link target.
        target: String,
    },
    /// A directory.
    Dir {
        /// Tree id.
        sha: String,
        /// Direct children.
        entries: Vec<TreeItem>,
    },
    /// A submodule (gitlink).
    Submodule {
        /// Commit id the gitlink records.
        sha: String,
    },
}

/// One changed file between two commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Path after the change.
    pub filename: String,
    /// Path before a rename.
    pub previous_filename: Option<String>,
    /// `added`, `removed`, `modified`, `renamed` or `changed` (mode only).
    pub status: String,
    /// Added lines.
    pub additions: u64,
    /// Removed lines.
    pub deletions: u64,
    /// Unified diff hunks (absent for binary files).
    pub patch: Option<String>,
    /// Blob id after the change (before it, for removals).
    pub sha: String,
}

/// `base...head`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Merge base commit, when one exists.
    pub merge_base: Option<String>,
    /// Commits reachable from head but not base, oldest first.
    pub ahead: Vec<GitCommitInfo>,
    /// How many commits base has that head lacks.
    pub behind_by: u64,
    /// Files changed from the merge base to head.
    pub files: Vec<FileChange>,
}

/// An entry of `POST /git/trees`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTreeItem {
    /// Path (may contain `/`).
    pub path: String,
    /// Git mode.
    pub mode: String,
    /// Object id; `None` with `content: None` deletes the path.
    pub sha: Option<String>,
    /// Inline file content (instead of `sha`).
    pub content: Option<Vec<u8>>,
}

/// `POST /git/commits`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewGitCommit {
    /// Message.
    pub message: String,
    /// Tree id.
    pub tree: String,
    /// Parent ids.
    pub parents: Vec<String>,
    /// Author; the caller when absent.
    pub author: Option<Signature>,
}

/// One file of a contents-API commit; `content: None` deletes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEdit {
    /// Path.
    pub path: String,
    /// New bytes, or `None` to delete.
    pub content: Option<Vec<u8>>,
}

/// Who performs a VCS write: the verified principal, its delegation chain and the bee
/// token, so the implementation can authorize and attribute it like any other write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    /// Acting principal id.
    pub principal_id: String,
    /// Delegation chain, actor first.
    pub chain: Vec<String>,
    /// The bee token the request carried.
    pub token: String,
    /// Display name for commit identities.
    pub name: String,
    /// Email for commit identities.
    pub email: String,
}

/// The VCS seen through git ids. Reads return `Ok(None)` for unknown objects; writes may
/// refuse with an [`crate::error::ApiError`] carrying the SPEC reason.
#[async_trait]
pub trait GitPort: Send + Sync {
    /// Refs whose git name starts with `prefix` (`refs/heads/`, `refs/tags/`, `refs/`), sorted.
    async fn refs(&self, repo: &Repo, prefix: &str) -> ApiResult<Vec<GitRefInfo>>;
    /// The commit a branch, tag, full git name or (abbreviated) commit id names.
    async fn resolve(&self, repo: &Repo, rev: &str) -> ApiResult<Option<String>>;
    /// A commit.
    async fn commit(&self, repo: &Repo, sha: &str) -> ApiResult<Option<GitCommitInfo>>;
    /// Commits reachable from `head`, newest first, touching `path` when given, after
    /// skipping `skip`, at most `limit`.
    async fn log(
        &self,
        repo: &Repo,
        head: &str,
        path: Option<&str>,
        skip: usize,
        limit: usize,
    ) -> ApiResult<Vec<GitCommitInfo>>;
    /// A tree by tree id, or the root tree of a commit id.
    async fn tree(&self, repo: &Repo, sha: &str, recursive: bool)
    -> ApiResult<Option<TreeListing>>;
    /// Blob bytes.
    async fn blob(&self, repo: &Repo, sha: &str) -> ApiResult<Option<Vec<u8>>>;
    /// What `path` names at commit `commit` (`""` is the root directory).
    async fn path(&self, repo: &Repo, commit: &str, path: &str) -> ApiResult<Option<PathEntry>>;
    /// Compares two commits.
    async fn compare(&self, repo: &Repo, base: &str, head: &str) -> ApiResult<Comparison>;
    /// Files changed from `from` to `to`.
    async fn diff(&self, repo: &Repo, from: &str, to: &str) -> ApiResult<Vec<FileChange>>;
    /// Stores a blob and returns its id.
    async fn create_blob(&self, repo: &Repo, actor: &Actor, bytes: Vec<u8>) -> ApiResult<String>;
    /// Builds a tree (on `base_tree` when given) and returns its id.
    async fn create_tree(
        &self,
        repo: &Repo,
        actor: &Actor,
        base_tree: Option<&str>,
        items: Vec<NewTreeItem>,
    ) -> ApiResult<String>;
    /// Creates a commit object and returns its id (no ref moves).
    async fn create_commit(&self, repo: &Repo, actor: &Actor, c: NewGitCommit)
    -> ApiResult<String>;
    /// Points the full git ref `name` at `sha` (`None` deletes it). Without `force` the move
    /// must be a fast-forward; `expected` (when given) must be the current target.
    async fn update_ref(
        &self,
        repo: &Repo,
        actor: &Actor,
        name: &str,
        sha: Option<&str>,
        expected: Option<&str>,
        force: bool,
    ) -> ApiResult<()>;
    /// Commits `edits` on top of `branch` (whose tip must be `parent` when given) and moves
    /// the branch; returns the new commit id.
    async fn commit_files(
        &self,
        repo: &Repo,
        actor: &Actor,
        branch: &str,
        parent: Option<&str>,
        edits: Vec<FileEdit>,
        message: &str,
    ) -> ApiResult<String>;
    /// The bee changeset of a git commit id.
    async fn changeset(&self, repo: &Repo, sha: &str) -> ApiResult<Option<ObjectId>>;
    /// The git commit id of a bee changeset.
    async fn git_id(&self, repo: &Repo, changeset: &ObjectId) -> ApiResult<Option<String>>;
}

/// Maps a presented credential to a bee token.
#[async_trait]
pub trait TokenMap: Send + Sync {
    /// The bee token for `presented` (a PAT, an app installation token, a Biscuit), or
    /// `None` when it is unknown.
    async fn bee_token(&self, presented: &str) -> Option<String>;
    /// A token for unauthenticated requests (public reads), when the server allows them.
    async fn anonymous(&self) -> Option<String> {
        None
    }
}

/// The identity mapping: the presented token is the bee token (Biscuits work as-is).
#[derive(Clone, Copy, Debug, Default)]
pub struct PassThroughTokens;

#[async_trait]
impl TokenMap for PassThroughTokens {
    async fn bee_token(&self, presented: &str) -> Option<String> {
        Some(presented.to_string())
    }
}

/// Small documents only the gateway keeps, by key.
#[async_trait]
pub trait CompatState: Send + Sync {
    /// The value at `key`.
    async fn get(&self, key: &str) -> ApiResult<Option<String>>;
    /// Sets `key`.
    async fn put(&self, key: &str, value: &str) -> ApiResult<()>;
    /// Removes `key`.
    async fn delete(&self, key: &str) -> ApiResult<()>;
}

/// Where release asset bytes go.
#[async_trait]
pub trait AssetStore: Send + Sync {
    /// Stores `bytes` at the signed `upload_url` the forge issued, with its headers.
    async fn put(
        &self,
        upload_url: &str,
        headers: &[(String, String)],
        bytes: Bytes,
    ) -> ApiResult<()>;
}

/// One code search hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeHit {
    /// Repo id.
    pub repo_id: String,
    /// File path.
    pub path: String,
    /// Blob id (hex).
    pub sha: String,
    /// Matching lines.
    pub fragments: Vec<String>,
}

/// Code search over the default branches of `repos`.
#[async_trait]
pub trait CodeSearch: Send + Sync {
    /// Files matching `query` (GitHub code search terms; qualifiers already removed).
    async fn search(&self, repos: &[Repo], query: &str, limit: usize) -> ApiResult<Vec<CodeHit>>;
}

/// Moves refs for the git data and contents APIs. The server wires its own write path, so
/// a ref moved through the gateway is authorized by policy and emits the ref events that
/// start CI and deliver webhooks; without one, [`crate::memory::HistoryGit`] compare-and-swaps
/// the VCS store directly and emits nothing.
#[async_trait]
pub trait RefMover: Send + Sync {
    /// Moves bee ref `name` of `repo` to `target` (`None` deletes) as `actor`, if its
    /// version is `expected` (`None` = the ref does not exist). `force` allows a move that
    /// is not a fast-forward.
    async fn move_ref(
        &self,
        repo: &Repo,
        actor: &Actor,
        name: &str,
        target: Option<&ObjectId>,
        expected: Option<u64>,
        force: bool,
    ) -> ApiResult<()>;
}
