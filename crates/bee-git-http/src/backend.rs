//! The seam between the gateway and the server that mounts it.
//!
//! The gateway (layer 6) cannot depend on `bee-server` (layer 7). The server implements
//! [`GitBackend`] over its own state: token verification, the repo gate, policy, push
//! protection, the audit log, and the Land coalescer. Everything git-specific (pkt-lines,
//! negotiation, packs, the git ↔ bee mapping) lives on this side and reads the stores
//! through the session's [`History`].

use std::sync::Arc;

use async_trait::async_trait;
use bee_core::{ObjectId, RefName};
use bee_history::History;
use bee_object::Changeset;

use crate::error::{GitError, GitResult};
use crate::locks::LockStore;
use crate::ssh::SshIdentity;

/// What a request is going to do with the repo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Clone, fetch, LFS download.
    Read,
    /// Push, LFS upload.
    Write,
}

/// A request to open a repo.
#[derive(Clone, Copy, Debug)]
pub struct OpenRequest<'a> {
    /// The owner (org) segment of the URL.
    pub owner: &'a str,
    /// The repo segment of the URL, without `.git`.
    pub repo: &'a str,
    /// The credential: the password of HTTP Basic (or its user name when the password is
    /// empty), or a Bearer token. `None` when the request carried none.
    pub token: Option<&'a str>,
    /// The intended access.
    pub access: Access,
    /// How the client reached the gateway.
    pub via: Via<'a>,
}

/// The transport and client of a request, for the repo's gateway settings (`git_enabled`,
/// `lfs_enabled`, `ssh_enabled`, `min_client_version`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Via<'a> {
    /// The client's `git/<version>` agent, when it sent one (HTTP `User-Agent`).
    pub agent: Option<&'a str>,
    /// A Git LFS request (batch or locks API, `git-lfs-authenticate`).
    pub lfs: bool,
    /// Over the SSH transport.
    pub ssh: bool,
}

/// What `git-lfs-authenticate` answers: where the LFS API is and how to authenticate to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LfsCredential {
    /// The LFS endpoint (`http://host/<owner>/<repo>.git/info/lfs`).
    pub href: String,
    /// A bearer token for it.
    pub token: String,
    /// Seconds the token stays valid.
    pub expires_in: u64,
}

/// A ref's state after a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefState {
    /// The target (`None` after a delete).
    pub target: Option<ObjectId>,
    /// The ref version (SPEC §5).
    pub version: u64,
}

/// Per-push context handed to the commit checks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushContext {
    /// The push options (`git push -o <opt>`), in order.
    pub options: Vec<String>,
    /// The push-protection bypass reason, from the push option
    /// `push-protection-bypass=<false_positive|used_in_tests|will_fix_later>`.
    pub bypass: Option<String>,
}

impl PushContext {
    /// A context from the push options.
    pub fn from_options(options: Vec<String>) -> PushContext {
        let bypass = options
            .iter()
            .rev()
            .find_map(|o| o.strip_prefix("push-protection-bypass="))
            .map(str::to_string);
        PushContext { options, bypass }
    }
}

/// The server side of the gateway.
#[async_trait]
pub trait GitBackend: Send + Sync + 'static {
    /// Authenticates the credential and opens the repo for `access`. Fails with
    /// `UNAUTHENTICATED` (and the token reasons) when the credential is missing or does not
    /// verify, `NOT_FOUND` for an unknown repo, and `PERMISSION_DENIED` / `POLICY_DENIED`
    /// when the principal may not read (or write) it.
    async fn open(&self, req: OpenRequest<'_>) -> GitResult<Arc<dyn RepoSession>>;

    /// The principal an SSH public key authenticates as, by its `SHA256:…` fingerprint
    /// (`None`: no such key registered, or its principal may not authenticate). Backends
    /// without an SSH key store keep this default, which refuses every key.
    async fn ssh_principal(&self, _fingerprint: &str) -> GitResult<Option<SshIdentity>> {
        Ok(None)
    }

    /// As [`GitBackend::ssh_principal`], with the key itself (`public_key` is its OpenSSH
    /// line): backends that accept per-repo keys (deploy keys) match on the key material. The
    /// default asks [`GitBackend::ssh_principal`].
    async fn ssh_principal_for_key(
        &self,
        fingerprint: &str,
        public_key: &str,
    ) -> GitResult<Option<SshIdentity>> {
        let _ = public_key;
        self.ssh_principal(fingerprint).await
    }

    /// As [`GitBackend::open`], for a principal that proved its identity with an SSH key
    /// instead of presenting a token: the same gate, policy and quotas apply.
    async fn open_as(
        &self,
        _principal: &str,
        _owner: &str,
        _repo: &str,
        _access: Access,
    ) -> GitResult<Arc<dyn RepoSession>> {
        Err(GitError::new(
            bee_core::Reason::Unimplemented,
            "this server has no SSH transport",
        ))
    }

    /// The HTTP credential git-lfs needs for an SSH remote (`git-lfs-authenticate`): a token for
    /// the principal good for this repo for about an hour, and the LFS endpoint it applies to.
    async fn ssh_lfs_credential(
        &self,
        _principal: &str,
        _owner: &str,
        _repo: &str,
    ) -> GitResult<LfsCredential> {
        Err(GitError::new(
            bee_core::Reason::Unimplemented,
            "this server has no SSH transport",
        ))
    }

    /// The history of a repo without a credential, for requests authorized by a signed URL
    /// the gateway issued to an authenticated principal (LFS transfers): `principal` is the
    /// one the URL was signed for and `write` is set for an upload. The backend decides
    /// again whether that principal may still read (or write) the repo, so a repo deleted,
    /// archived (uploads) or taken out of the principal's reach since the URL was issued
    /// refuses it. `NOT_FOUND` for an unknown repo or one the principal cannot see.
    async fn signed_history(
        &self,
        owner: &str,
        repo: &str,
        principal: &str,
        write: bool,
    ) -> GitResult<History>;

    /// The key that signs LFS transfer URLs (at least 32 bytes, stable across processes).
    fn url_key(&self) -> Vec<u8>;

    /// The base URL clients reach this server at (`http://host:port`).
    fn public_url(&self) -> String;

    /// The current time, from the server's clock.
    fn now_ms(&self) -> u64;

    /// Where Git LFS locks are kept.
    fn locks(&self) -> &dyn LockStore;
}

/// One authenticated principal's view of one repo, for the length of a request.
#[async_trait]
pub trait RepoSession: Send + Sync {
    /// The repo's stores, with the server's eager derived rows (`history.eager()`).
    fn history(&self) -> &History;

    /// The authenticated principal's id (Meta `creator`, uploader, workspace owner).
    fn principal(&self) -> &str;

    /// The principal's name as people see it (lock owners, `git lfs locks`).
    fn principal_label(&self) -> String;

    /// The repo's full name, `org/name`.
    fn full_name(&self) -> &str;

    /// Whether the principal may write the path (token path scope and policy): the
    /// condition for locking it.
    fn path_writable(&self, path: &str) -> bool;

    /// Whether the principal may break other people's locks (the `force_update` permission
    /// on the repo).
    async fn may_force_unlock(&self) -> GitResult<bool>;

    /// Whether the principal may see this ref (token ref scope).
    fn ref_visible(&self, name: &RefName) -> bool;

    /// The largest blob the repo accepts.
    fn max_blob_bytes(&self) -> u64;

    /// A number that changes whenever content the repo served may have become unservable
    /// (its redaction epoch, SPEC §13). Cached packs are keyed by it, so a pack built before a
    /// redaction is never served after it. The default never changes.
    fn content_generation(&self) -> u64 {
        0
    }

    /// Every check CreateChangeset makes on a changeset whose objects are uploaded but which
    /// is not inserted yet: write access to each changed path, push rules, object
    /// availability, push protection (with the context's bypass, audited) and budgets.
    async fn check_commit(&self, changeset: &Changeset, push: &PushContext) -> GitResult<()>;

    /// Records that the changeset was created (audit log).
    async fn committed(&self, changeset: &ObjectId) -> GitResult<()>;

    /// Whether the ref is protected (moves only through Land).
    async fn is_protected(&self, name: &RefName) -> GitResult<bool>;

    /// Allows a hosted backend to keep a publication permit through a multi-ref atomic
    /// push, including any needed rollback of earlier writes.
    async fn atomic_publication(&self) -> GitResult<Option<Box<dyn Send + Sync>>> {
        Ok(None)
    }

    /// Lands the stack ending at `head` on a protected ref through the coalescer, under
    /// policy (SPEC §7.4). The result names the ref's new target, which differs from `head`
    /// when the stack was rebased onto a moved tip.
    async fn land(&self, name: &RefName, head: &ObjectId) -> GitResult<RefState>;

    /// Creates, moves (`force` permits a non-fast-forward move, under the `force`
    /// permission) or deletes (`target = None`) a non-protected ref with a compare-and-swap
    /// on `expected_version` (0: the ref must not exist).
    async fn write_ref(
        &self,
        name: &RefName,
        expected_version: u64,
        target: Option<ObjectId>,
        force: bool,
    ) -> GitResult<RefState>;
}
