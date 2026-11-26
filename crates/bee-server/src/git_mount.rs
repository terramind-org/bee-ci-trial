//! The git gateway (`bee-git-http`, PLAN W16) mounted on this server's listener: smart HTTP
//! and Git LFS under `/<owner>/<repo>.git/…`, backed by the same token verification, repo gate,
//! policy, push protection, budgets, audit log and Land coalescer as every RPC.
//!
//! The gateway does everything git-specific; this module only answers its questions through
//! [`AppState`] and the in-process `WriteService` / `RefService` handlers, so a git push is
//! authorized, audited and landed exactly like the equivalent API calls. See
//! `docs/spec/git-gateway.md`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bee_auth::Request as AuthRequest;
use bee_core::{ObjectId, RefName, RefNamespace};
use bee_git_http::locks::{Lock, LockStore};
use bee_git_http::ssh::SshIdentity;
use bee_git_http::{
    Access, GatewayConfig, GitBackend, GitError, GitGateway, GitResult, LfsCredential, OpenRequest,
    PushContext, RefState, RepoSession, Via,
};
use bee_history::History;
use bee_object::Changeset;
use bee_policy::{Action, Policy};
use bee_proto::v1::{self, ref_service_server::RefService, write_service_server::WriteService};
use serde_json::json;
use tonic::metadata::MetadataMap;

use crate::auth::{Caller, Usage};
use crate::convert::{id_from, id_to};
use crate::errors::ApiError;
use crate::kv::{Kv, KvOp};
use crate::services::refs::Refs;
use crate::services::write::{BYPASS_HEADER, Writes};
use crate::state::{AppState, RepoCtx, RequestMeta};

/// The gateway's routes over `state`, to merge into the HTTP router.
pub fn router(state: Arc<AppState>) -> crate::errors::ApiResult<axum::Router> {
    Ok(gateway(state)?.router())
}

/// The gateway over `state`: the HTTP routes and the SSH listener share one gateway (and its
/// pack cache).
pub fn gateway(state: Arc<AppState>) -> crate::errors::ApiResult<Arc<GitGateway>> {
    let base = crate::server::url_key(&state.cfg)?;
    let url_key = blake3::derive_key("bee git-lfs transfer urls v1", &base).to_vec();
    let locks = KvLocks {
        kv: state.kv.clone(),
    };
    let backend = Arc::new(ServerGit {
        state,
        url_key,
        locks,
    });
    let metrics = backend.state.metrics.git.clone();
    Ok(GitGateway::with_metrics(
        backend,
        GatewayConfig::default(),
        metrics,
    ))
}

/// Lifetime of the token minted for one SSH exec session.
const SSH_SESSION_SECS: u64 = 600;
/// Lifetime of the token `git-lfs-authenticate` hands to git-lfs.
const LFS_TOKEN_SECS: u64 = 3600;

/// SSH identities that are deploy keys carry this prefix and the key's stored fingerprint;
/// the repo (and so the key's own principal `deploy-key:<id>`) is known only at exec.
const DEPLOY_KEY_IDENTITY: &str = "ssh-deploy-key:";

impl ServerGit {
    /// The deploy key of `owner/repo` matching a deploy-key SSH identity, with the repo.
    async fn deploy_key(
        &self,
        identity: &str,
        owner: &str,
        repo: &str,
    ) -> GitResult<(crate::state::RepoCtx, v1::DeployKey)> {
        let s = &self.state;
        let fp = identity.trim_start_matches(DEPLOY_KEY_IDENTITY);
        let ctx = s
            .repo(repo_ref(&format!("{owner}/{repo}")).as_ref())
            .await
            .map_err(git_error)?;
        let not_here = || {
            GitError::named(
                "PERMISSION_DENIED",
                format!("this deploy key is not registered on {owner}/{repo}"),
            )
        };
        let w = s.hub.get().ok_or_else(not_here)?;
        let keys = w
            .forge
            .orgs
            .deploy_keys(&bee_forge::Cx::system(), &ctx.id_text())
            .await
            .map_err(|e| git_error(crate::forge::forge_to_api(e)))?;
        let key = keys
            .into_iter()
            .find(|k| {
                bee_forge::orgs::Orgs::deploy_key_fingerprint(&k.public_key).as_deref() == Some(fp)
            })
            .ok_or_else(not_here)?;
        Ok((ctx, key))
    }

    /// A token for a deploy key's own service principal (`deploy-key:<id>`), limited to its
    /// repo: read, plus commit and land unless the key is read-only. Its forge role is the
    /// key's (read or write), so the repo gate decides the rest.
    fn mint_deploy(
        &self,
        ctx: &crate::state::RepoCtx,
        key: &v1::DeployKey,
        secs: u64,
    ) -> GitResult<String> {
        let s = &self.state;
        let now = s.now_ms() / 1000;
        let mut actions = vec![Action::Read];
        if !key.read_only {
            actions.extend([Action::Commit, Action::Land]);
        }
        let scope = bee_auth::Scope::new()
            .repos(std::slice::from_ref(&ctx.full_name))
            .map_err(|e| git_error(ApiError::from(e)))?
            .actions(actions)
            .expires_at(now + secs)
            .session(format!("ssh:{}", key.id));
        let principal = bee_auth::Principal::service(
            key.principal_id.clone(),
            format!("deploy key {}", key.title),
        );
        s.ring
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .mint(
                &bee_auth::MintRequest {
                    principal,
                    on_behalf_of: Vec::new(),
                    scope,
                },
                now,
            )
            .map(|t| t.text)
            .map_err(|e| git_error(ApiError::from(e)))
    }

    /// Opens a session for `token` (the request's credential, or the anonymous read token).
    async fn open_as(&self, req: OpenRequest<'_>, token: &str) -> GitResult<Arc<dyn RepoSession>> {
        let s = &self.state;
        let mut md = MetadataMap::new();

        let value = format!("Bearer {token}")
            .parse()
            .map_err(|_| GitError::unauthenticated("the credential is not a valid token"))?;
        md.insert("authorization", value);
        let caller = s.caller(&md).await.map_err(git_error)?;
        let repo = s
            .repo(repo_ref(&format!("{}/{}", req.owner, req.repo)).as_ref())
            .await
            .map_err(git_error)?;
        let action = match req.access {
            Access::Read => Action::Read,
            Access::Write => Action::Commit,
        };
        let policy = s
            .gate(&caller, &repo, action, None)
            .await
            .map_err(git_error)?;
        if req.access == Access::Read && !reads_everything(s, &caller, &repo, &policy) {
            return Err(GitError::named(
                "POLICY_DENIED",
                "git serves whole trees: this principal may not read every path of the repo \
                 (use the API, which filters restricted paths)",
            ));
        }
        check_gateway(s, &repo, req.via).await?;
        let meta = RequestMeta {
            request_id: crate::util::random_hex(8),
            deadline: Instant::now() + Duration::from_millis(s.cfg.limits.max_deadline_ms),
            method: match req.access {
                Access::Read => "git-upload-pack".into(),
                Access::Write => "git-receive-pack".into(),
            },
        };
        let history = s.history(&repo);
        let redaction_epoch = s.redaction_epoch(&repo).await.map_err(git_error)?;
        Ok(Arc::new(ServerSession {
            redaction_epoch,
            state: s.clone(),
            caller,
            repo,
            policy,
            history,
            md,
            meta,
            push_options: std::sync::Mutex::new(Vec::new()),
            atomic_fence: std::sync::OnceLock::new(),
        }))
    }

    /// A token for a principal that authenticated with an SSH key (the key proved who is
    /// connecting; the token carries that through the same gate, policy and quotas as an HTTP
    /// request). Refused for a principal that is gone or deactivated.
    async fn mint_for(&self, principal: &str, scope: bee_auth::Scope) -> GitResult<String> {
        let s = &self.state;
        let rec = s
            .principal(principal)
            .await
            .map_err(git_error)?
            .filter(|r| !r.deactivated)
            .ok_or_else(|| GitError::unauthenticated("this key's principal cannot sign in"))?;
        let now = s.now_ms() / 1000;
        let token = s
            .ring
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .mint(
                &bee_auth::MintRequest {
                    principal: rec.to_auth(),
                    on_behalf_of: Vec::new(),
                    scope,
                },
                now,
            )
            .map_err(|e| git_error(ApiError::from(e)))?;
        Ok(token.text)
    }
}

/// Opens a gateway session for `token` on `full_name` (`org/repo`), exactly as the smart
/// HTTP routes do (`GitService` uses it for its RPCs).
pub(crate) async fn open_session(
    state: &Arc<AppState>,
    token: &str,
    full_name: &str,
    access: Access,
) -> GitResult<Arc<dyn RepoSession>> {
    let (owner, repo) = full_name
        .split_once('/')
        .ok_or_else(|| GitError::invalid("repo full_name must be `org/name`"))?;
    let base = crate::server::url_key(&state.cfg).map_err(git_error)?;
    let url_key = blake3::derive_key("bee git-lfs transfer urls v1", &base).to_vec();
    let backend = ServerGit {
        state: state.clone(),
        url_key,
        locks: KvLocks {
            kv: state.kv.clone(),
        },
    };
    backend
        .open(OpenRequest {
            owner,
            repo,
            token: Some(token),
            access,
            via: Via::default(),
        })
        .await
}

/// The repo's gateway settings applied to one request (`docs/spec/operations.md` §9): git off
/// is `GIT_DISABLED` on every transport, SSH off refuses SSH sessions, LFS off refuses the LFS
/// API, and a `git/<version>` agent older than `min_client_version` (default 2.30.0) is refused.
pub(crate) async fn check_gateway(s: &AppState, repo: &RepoCtx, via: Via<'_>) -> GitResult<()> {
    use crate::services::vcs_gateway::{
        DEFAULT_MIN_CLIENT_VERSION, git_enabled, parse_version, settings_of,
    };
    if !git_enabled(s, repo).await {
        return Err(GitError::named(
            "GIT_DISABLED",
            format!("git is disabled for {}", repo.full_name),
        ));
    }
    let st = settings_of(s, repo).await.map_err(git_error)?;
    if via.ssh && !st.ssh_enabled {
        return Err(GitError::named(
            "GIT_DISABLED",
            format!(
                "SSH is disabled for {} (ssh_disabled): use https",
                repo.full_name
            ),
        ));
    }
    if via.lfs && !st.lfs_enabled {
        return Err(GitError::named(
            "INVALID_STATE",
            format!("Git LFS is disabled for {} (lfs_disabled)", repo.full_name),
        ));
    }
    if let Some(agent) = via.agent {
        let min = if st.min_client_version.is_empty() {
            DEFAULT_MIN_CLIENT_VERSION
        } else {
            st.min_client_version.as_str()
        };
        if let (Some(have), Some(need)) = (parse_version(agent), parse_version(min))
            && have < need
        {
            return Err(GitError::named(
                "INVALID_STATE",
                format!(
                    "{agent} is older than git {min}, the oldest {} accepts (git_too_old): upgrade git",
                    repo.full_name
                ),
            ));
        }
    }
    Ok(())
}

/// True when the last paragraph of a commit message has a `Change-Id:` trailer with a value.
fn has_change_id_trailer(message: &str) -> bool {
    let trimmed = message.trim_end();
    let last = trimmed.rsplit("\n\n").next().unwrap_or(trimmed);
    last.lines().any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("change-id") && !v.trim().is_empty()
        })
    })
}

/// The key that signs LFS transfer URLs.
pub(crate) fn lfs_url_key(state: &AppState) -> crate::errors::ApiResult<Vec<u8>> {
    let base = crate::server::url_key(&state.cfg)?;
    Ok(blake3::derive_key("bee git-lfs transfer urls v1", &base).to_vec())
}

fn git_error(e: ApiError) -> GitError {
    git_detail(e.detail.reason, e.detail.subcode, e.detail.message)
}

fn git_detail(reason: String, subcode: String, message: String) -> GitError {
    // Stock Git can display only text, so carry the safety subcode in that text.
    let message = if matches!(
        subcode.as_str(),
        "safety_merge_frozen" | "safety_forge_unavailable"
    ) {
        format!("{subcode}: {message}")
    } else {
        message
    };
    GitError::named(reason, message)
}

fn status_error(s: tonic::Status) -> GitError {
    match bee_proto::errors::detail_from_status(&s) {
        Some(d) => git_detail(d.reason, d.subcode, d.message),
        None => GitError::named(
            bee_proto::errors::reason_name(bee_proto::errors::default_reason(s.code())),
            s.message().to_string(),
        ),
    }
}

fn repo_ref(full_name: &str) -> Option<v1::RepoRef> {
    Some(v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::FullName(full_name.to_string())),
    })
}

struct ServerGit {
    state: Arc<AppState>,
    url_key: Vec<u8>,
    locks: KvLocks,
}

#[async_trait]
impl GitBackend for ServerGit {
    async fn open(&self, req: OpenRequest<'_>) -> GitResult<Arc<dyn RepoSession>> {
        let s = &self.state;
        // A clone or fetch without credentials reads public repos as the anonymous read token
        // (`crate::access`). Any refusal of it asks for credentials, so git prompts for them
        // (and a private repo looks the same as a missing one).
        let anonymous_token;
        let (token, anonymous) = match req.token {
            Some(t) => (t, false),
            None if req.access == Access::Read => {
                anonymous_token = s.anonymous_token().map_err(git_error)?.ok_or_else(|| {
                    GitError::unauthenticated(
                        "credentials required: use your bee token as the HTTP password",
                    )
                })?;
                (anonymous_token.as_str(), true)
            }
            None => {
                return Err(GitError::unauthenticated(
                    "credentials required: use your bee token as the HTTP password",
                ));
            }
        };
        match self.open_as(req, token).await {
            Err(e)
                if anonymous
                    && matches!(
                        e.reason.as_str(),
                        "NOT_FOUND" | "UNAUTHENTICATED" | "PERMISSION_DENIED" | "POLICY_DENIED"
                    ) =>
            {
                Err(GitError::unauthenticated(
                    "credentials required: use your bee token as the HTTP password",
                ))
            }
            r => r,
        }
    }

    async fn ssh_principal(&self, fingerprint: &str) -> GitResult<Option<SshIdentity>> {
        crate::git_ssh::identity_of(&self.state, fingerprint)
            .await
            .map_err(git_error)
    }

    async fn ssh_principal_for_key(
        &self,
        fingerprint: &str,
        public_key: &str,
    ) -> GitResult<Option<SshIdentity>> {
        if let Some(id) = self.ssh_principal(fingerprint).await? {
            return Ok(Some(id));
        }
        // Not an account key: a deploy key of some repo (F416-029)?
        let Some(w) = self.state.hub.get() else {
            return Ok(None);
        };
        let found = w
            .forge
            .orgs
            .deploy_keys_by_material(&bee_forge::Cx::system(), public_key)
            .await
            .unwrap_or_default();
        let Some((_, key)) = found.first() else {
            return Ok(None);
        };
        let Some(fp) = bee_forge::orgs::Orgs::deploy_key_fingerprint(public_key) else {
            return Ok(None);
        };
        Ok(Some(SshIdentity {
            principal: format!("{DEPLOY_KEY_IDENTITY}{fp}"),
            name: format!("deploy key {}", key.title),
        }))
    }

    async fn open_as(
        &self,
        principal: &str,
        owner: &str,
        repo: &str,
        access: Access,
    ) -> GitResult<Arc<dyn RepoSession>> {
        // The key proved who is connecting; a token minted for that principal for this one
        // session carries it through the same gate, policy and quotas as an HTTP request.
        let token = if principal.starts_with(DEPLOY_KEY_IDENTITY) {
            let (ctx, key) = self.deploy_key(principal, owner, repo).await?;
            if access == Access::Write && key.read_only {
                return Err(GitError::named(
                    "PERMISSION_DENIED",
                    format!("deploy key {} is read-only", key.title),
                ));
            }
            self.mint_deploy(&ctx, &key, SSH_SESSION_SECS)?
        } else {
            let scope =
                bee_auth::Scope::new().expires_at(self.state.now_ms() / 1000 + SSH_SESSION_SECS);
            self.mint_for(principal, scope).await?
        };
        self.open(OpenRequest {
            owner,
            repo,
            token: Some(&token),
            access,
            via: Via {
                ssh: true,
                ..Via::default()
            },
        })
        .await
    }

    async fn ssh_lfs_credential(
        &self,
        principal: &str,
        owner: &str,
        repo: &str,
    ) -> GitResult<LfsCredential> {
        let ctx = self
            .state
            .repo(repo_ref(&format!("{owner}/{repo}")).as_ref())
            .await
            .map_err(git_error)?;
        check_gateway(
            &self.state,
            &ctx,
            Via {
                lfs: true,
                ssh: true,
                agent: None,
            },
        )
        .await?;
        let token = if principal.starts_with(DEPLOY_KEY_IDENTITY) {
            let (ctx, key) = self.deploy_key(principal, owner, repo).await?;
            self.mint_deploy(&ctx, &key, LFS_TOKEN_SECS)?
        } else {
            let scope = bee_auth::Scope::new()
                .repos([format!("{owner}/{repo}")])
                .map_err(|e| git_error(ApiError::from(e)))?
                .expires_at(self.state.now_ms() / 1000 + LFS_TOKEN_SECS);
            self.mint_for(principal, scope).await?
        };
        Ok(LfsCredential {
            href: format!("{}/{owner}/{repo}.git/info/lfs", self.state.public_url()),
            token,
            expires_in: LFS_TOKEN_SECS,
        })
    }

    async fn signed_history(
        &self,
        owner: &str,
        repo: &str,
        principal: &str,
        write: bool,
    ) -> GitResult<History> {
        let s = &self.state;
        let repo = s
            .repo(repo_ref(&format!("{owner}/{repo}")).as_ref())
            .await
            .map_err(git_error)?;
        // The URL proves who it was issued to, not that they may still use it: decide again.
        let chain = s.principal_chain(principal).await.map_err(git_error)?;
        let action = if write { Action::Commit } else { Action::Read };
        s.authorize_chain(&chain, &repo, action)
            .await
            .map_err(git_error)?;
        Ok(s.history(&repo))
    }

    fn url_key(&self) -> Vec<u8> {
        self.url_key.clone()
    }

    fn public_url(&self) -> String {
        self.state.public_url()
    }

    fn now_ms(&self) -> u64 {
        self.state.now_ms()
    }

    fn locks(&self) -> &dyn LockStore {
        &self.locks
    }
}

/// True when the caller may read every path: the root and every access-rule prefix, under
/// both the token's read paths and the policy.
fn reads_everything(s: &AppState, caller: &Caller, repo: &RepoCtx, policy: &Policy) -> bool {
    if caller
        .token
        .scope
        .read_paths
        .as_ref()
        .is_some_and(|p| !p.contains(""))
    {
        return false;
    }
    std::iter::once("")
        .chain(policy.doc().access.keys().map(String::as_str))
        .all(|p| s.path_allowed(caller, repo.repo.id, policy, Action::Read, p))
}

struct ServerSession {
    state: Arc<AppState>,
    caller: Caller,
    repo: RepoCtx,
    policy: Arc<Policy>,
    history: History,
    md: MetadataMap,
    meta: RequestMeta,
    /// The push's `-o` options, as seen by the first ingested changeset (`bee.cr`).
    push_options: std::sync::Mutex<Vec<String>>,
    /// The lifetime is owned by receive-pack, including its rollback path.
    atomic_fence: std::sync::OnceLock<std::sync::Weak<bee_forge::safety::control::LandFence>>,
    /// The repo's redaction epoch when the session opened (pack cache generation).
    redaction_epoch: u64,
}

impl ServerSession {
    fn request<T>(&self, msg: T, method: &str) -> tonic::Request<T> {
        let mut r = tonic::Request::new(msg);
        *r.metadata_mut() = self.md.clone();
        let mut meta = self.meta.clone();
        meta.method = format!("{}:{method}", self.meta.method);
        r.extensions_mut().insert(meta);
        r
    }

    fn ref_state(r: Option<v1::Ref>) -> GitResult<RefState> {
        let r = r.ok_or_else(|| GitError::internal("ref write returned no ref"))?;
        let target = match &r.target {
            Some(t) if !t.raw.is_empty() => Some(id_from(Some(t), "target").map_err(git_error)?),
            _ => None,
        };
        Ok(RefState {
            target,
            version: r.version,
        })
    }
}

#[async_trait]
impl RepoSession for ServerSession {
    fn history(&self) -> &History {
        &self.history
    }

    fn principal(&self) -> &str {
        self.caller.id()
    }

    fn principal_label(&self) -> String {
        self.caller
            .record
            .as_ref()
            .map(|r| r.display.clone())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| self.caller.id().to_string())
    }

    fn full_name(&self) -> &str {
        &self.repo.full_name
    }

    fn path_writable(&self, path: &str) -> bool {
        self.state.path_allowed(
            &self.caller,
            self.repo.repo.id,
            &self.policy,
            Action::Commit,
            path,
        )
    }

    async fn may_force_unlock(&self) -> GitResult<bool> {
        match self
            .state
            .gate(&self.caller, &self.repo, Action::ForceUpdate, None)
            .await
        {
            Ok(_) => Ok(true),
            Err(e)
                if e.detail.reason == "POLICY_DENIED" || e.detail.reason == "PERMISSION_DENIED" =>
            {
                Ok(false)
            }
            Err(e) => Err(git_error(e)),
        }
    }

    fn ref_visible(&self, name: &RefName) -> bool {
        let req = AuthRequest::new(self.repo.full_name.clone(), Action::Read).on_ref(name.as_str());
        self.state
            .token_check(&self.caller, &self.repo, req)
            .is_ok()
    }

    fn max_blob_bytes(&self) -> u64 {
        self.state.cfg.limits.max_blob_bytes
    }

    fn content_generation(&self) -> u64 {
        self.redaction_epoch
    }

    async fn check_commit(&self, changeset: &Changeset, push: &PushContext) -> GitResult<()> {
        let s = &self.state;
        if let Ok(mut o) = self.push_options.lock()
            && o.is_empty()
        {
            o.clone_from(&push.options);
        }
        let settings = crate::services::vcs_gateway::settings_of(s, &self.repo)
            .await
            .map_err(git_error)?;
        if settings.require_change_id_trailer {
            let message = self
                .history
                .meta_object(changeset.meta())
                .await
                .map_err(GitError::from)?
                .map(|m| m.message)
                .unwrap_or_default();
            if !has_change_id_trailer(&message) {
                return Err(GitError::invalid(format!(
                    "{} requires a `Change-Id:` trailer on every pushed commit \
                     (change_id_trailer_required)",
                    self.repo.full_name
                )));
            }
        }
        let delta = changeset.delta();
        let paths: Vec<String> = delta.iter().map(|e| e.path.to_string()).collect();
        s.require_paths(
            &self.caller,
            &self.repo,
            &self.policy,
            Action::Commit,
            &paths,
            None,
        )
        .map_err(git_error)?;
        let writes = Writes::new(s.clone());
        writes
            .check_puts(&self.caller, &self.repo, &self.policy, delta)
            .await
            .map_err(git_error)?;
        let mut md = self.md.clone();
        if let Some(b) = &push.bypass {
            let v = b
                .parse()
                .map_err(|_| GitError::invalid("bad push-protection-bypass value"))?;
            md.insert(BYPASS_HEADER, v);
        }
        writes
            .scan_secrets(
                &self.caller,
                &self.repo,
                &self.policy,
                delta,
                &md,
                &self.meta,
            )
            .await
            .map_err(git_error)?;
        s.charge(
            &self.caller,
            Usage {
                changesets: 1,
                ..Usage::default()
            },
        )
        .await
        .map_err(git_error)
    }

    async fn committed(&self, changeset: &ObjectId) -> GitResult<()> {
        self.state
            .audit(
                &self.caller,
                &self.meta,
                Some(&self.repo),
                &changeset.to_text(),
                "commit",
                json!({ "via": "git" }),
            )
            .await
            .map(|_| ())
            .map_err(git_error)
    }

    async fn is_protected(&self, name: &RefName) -> GitResult<bool> {
        if name.namespace() == RefNamespace::Protected {
            return Ok(true);
        }
        self.state
            .hub
            .queue_or_policy_protected(&self.state, &self.repo, name.as_str())
            .await
            .map_err(git_error)
    }

    async fn atomic_publication(&self) -> GitResult<Option<Box<dyn Send + Sync>>> {
        let fence = self
            .state
            .hub
            .fence_ref_publication(
                &self.state,
                &self.repo,
                self.caller.token.principal.kind != bee_policy::PrincipalKind::Human,
            )
            .await
            .map_err(git_error)?;
        let Some(fence) = fence else {
            return Ok(None);
        };
        self.atomic_fence
            .set(Arc::downgrade(&fence))
            .map_err(|_| GitError::internal("Git publication already started"))?;
        Ok(Some(Box::new(fence)))
    }

    async fn land(&self, name: &RefName, head: &ObjectId) -> GitResult<RefState> {
        let writes = Writes::new(self.state.clone());
        let req = self.request(
            v1::LandRequest {
                repo: repo_ref(&self.repo.full_name),
                r#ref: name.as_str().to_string(),
                head: Some(id_to(head)),
                ..Default::default()
            },
            "Land",
        );
        let resp = writes.land(req).await.map_err(status_error)?.into_inner();
        Self::ref_state(resp.r#ref)
    }

    async fn write_ref(
        &self,
        name: &RefName,
        expected_version: u64,
        target: Option<ObjectId>,
        force: bool,
    ) -> GitResult<RefState> {
        let out = self
            .write_ref_raw(name, expected_version, target, force)
            .await?;
        // Change requests follow pushed refs; `push.autoChange` opens one for a new branch.
        let options = self
            .push_options
            .lock()
            .map(|o| o.clone())
            .unwrap_or_default();
        self.state
            .forge
            .ref_moved(
                &self.repo,
                name.as_str(),
                out.target.as_ref(),
                &self.caller,
                &options,
                true,
            )
            .await;
        Ok(out)
    }
}

/// Builds and caches, in the gateway's pack cache, the pack a stock `git clone` of `repo`
/// asks for (`land.warm_pack`), so the first clone after a land is served from memory.
///
/// The clone is seen as a reader whose token sees every ref and who owns no workspace: a
/// clone's wants are its visible branches and tags, so this matches every such reader (a
/// principal with workspaces of its own also wants those, and plans its own pack). Failures
/// are dropped: the clone that needs the pack builds it.
pub(crate) async fn warm_clone_pack(state: &Arc<AppState>, repo: &RepoCtx) {
    let Some(gw) = state.git_gateway.get() else {
        return;
    };
    if check_gateway(state, repo, Via::default()).await.is_err() {
        return;
    }
    let Ok(redaction_epoch) = state.redaction_epoch(repo).await else {
        return;
    };
    let session = WarmSession {
        history: state.history(repo),
        full_name: repo.full_name.clone(),
        redaction_epoch,
    };
    let _ = gw.prewarm_clone(&session).await;
}

/// The read-only session post-land warming plans a clone through (see [`warm_clone_pack`]).
struct WarmSession {
    history: History,
    full_name: String,
    redaction_epoch: u64,
}

impl WarmSession {
    fn refused() -> GitError {
        GitError::named(
            "PERMISSION_DENIED",
            "the post-land warming session only reads",
        )
    }
}

#[async_trait]
impl RepoSession for WarmSession {
    fn history(&self) -> &History {
        &self.history
    }

    /// No principal: an empty id owns no workspace (`ws/<owner>/…` never has an empty owner).
    fn principal(&self) -> &str {
        ""
    }

    fn principal_label(&self) -> String {
        String::new()
    }

    fn full_name(&self) -> &str {
        &self.full_name
    }

    fn path_writable(&self, _path: &str) -> bool {
        false
    }

    async fn may_force_unlock(&self) -> GitResult<bool> {
        Ok(false)
    }

    fn ref_visible(&self, _name: &RefName) -> bool {
        true
    }

    fn max_blob_bytes(&self) -> u64 {
        0
    }

    fn content_generation(&self) -> u64 {
        self.redaction_epoch
    }

    async fn check_commit(&self, _changeset: &Changeset, _push: &PushContext) -> GitResult<()> {
        Err(Self::refused())
    }

    async fn committed(&self, _changeset: &ObjectId) -> GitResult<()> {
        Err(Self::refused())
    }

    async fn is_protected(&self, _name: &RefName) -> GitResult<bool> {
        Ok(true)
    }

    async fn land(&self, _name: &RefName, _head: &ObjectId) -> GitResult<RefState> {
        Err(Self::refused())
    }

    async fn write_ref(
        &self,
        _name: &RefName,
        _expected_version: u64,
        _target: Option<ObjectId>,
        _force: bool,
    ) -> GitResult<RefState> {
        Err(Self::refused())
    }
}

impl ServerSession {
    async fn write_ref_raw(
        &self,
        name: &RefName,
        expected_version: u64,
        target: Option<ObjectId>,
        force: bool,
    ) -> GitResult<RefState> {
        // Receive-pack uses the same fenced ref CAS as native requests. Protected pushes
        // use Land above, whose coalescer holds the same publication permit.
        let refs = Refs::new(self.state.clone())
            .with_git_fence(self.atomic_fence.get().and_then(std::sync::Weak::upgrade));
        let repo = repo_ref(&self.repo.full_name);
        let reason = "git push".to_string();
        match target {
            None => {
                let req = self.request(
                    v1::DeleteRefRequest {
                        repo,
                        name: name.as_str().to_string(),
                        expected_version,
                        reason,
                    },
                    "DeleteRef",
                );
                refs.delete_ref(req).await.map_err(status_error)?;
                Ok(RefState {
                    target: None,
                    version: expected_version + 1,
                })
            }
            Some(t) if expected_version == 0 => {
                let req = self.request(
                    v1::CreateRefRequest {
                        repo,
                        name: name.as_str().to_string(),
                        target: Some(id_to(&t)),
                        reason,
                    },
                    "CreateRef",
                );
                let r = refs.create_ref(req).await.map_err(status_error)?;
                Self::ref_state(Some(r.into_inner()))
            }
            Some(t) => {
                let req = self.request(
                    v1::UpdateRefRequest {
                        repo,
                        name: name.as_str().to_string(),
                        target: Some(id_to(&t)),
                        expected_version,
                        allow_non_fast_forward: force,
                        reason,
                    },
                    "UpdateRef",
                );
                let r = refs.update_ref(req).await.map_err(status_error)?;
                Self::ref_state(Some(r.into_inner()))
            }
        }
    }
}

/// Namespace of Git LFS locks by path: `<repo>\0<path>` → the lock.
const NS_LOCK: &str = "lfs_lock";
/// Namespace of Git LFS locks by id: `<repo>\0<id>` → the path.
const NS_LOCK_ID: &str = "lfs_lock_id";

/// [`LockStore`] over the server's key-value tables: one atomic [`Kv::apply`] per change, so two
/// clients locking one path never both win.
struct KvLocks {
    kv: Arc<dyn Kv>,
}

fn lock_key(repo: &str, tail: &str) -> Vec<u8> {
    let mut k = repo.as_bytes().to_vec();
    k.push(0);
    k.extend_from_slice(tail.as_bytes());
    k
}

fn decode_lock(raw: &[u8]) -> GitResult<Lock> {
    serde_json::from_slice(raw).map_err(|e| GitError::internal(format!("corrupt lock row: {e}")))
}

#[async_trait]
impl LockStore for KvLocks {
    async fn create(&self, repo: &str, lock: &Lock) -> GitResult<Option<Lock>> {
        let by_path = lock_key(repo, &lock.path);
        let by_id = lock_key(repo, &lock.id);
        let row = serde_json::to_vec(lock).map_err(|e| GitError::internal(e.to_string()))?;
        for _ in 0..20 {
            if let Some(held) = self.kv.get(NS_LOCK, &by_path).await? {
                return decode_lock(&held).map(Some);
            }
            let won = self
                .kv
                .apply(vec![
                    KvOp::expect(NS_LOCK, by_path.clone(), None),
                    KvOp::expect(NS_LOCK_ID, by_id.clone(), None),
                    KvOp::put(NS_LOCK, by_path.clone(), row.clone()),
                    KvOp::put(NS_LOCK_ID, by_id.clone(), lock.path.as_bytes().to_vec()),
                ])
                .await?;
            if won {
                return Ok(None);
            }
        }
        Err(GitError::named("UNAVAILABLE", "lock table contended"))
    }

    async fn by_id(&self, repo: &str, id: &str) -> GitResult<Option<Lock>> {
        match self.kv.get(NS_LOCK_ID, &lock_key(repo, id)).await? {
            Some(path) => self.by_path(repo, &String::from_utf8_lossy(&path)).await,
            None => Ok(None),
        }
    }

    async fn by_path(&self, repo: &str, path: &str) -> GitResult<Option<Lock>> {
        match self.kv.get(NS_LOCK, &lock_key(repo, path)).await? {
            Some(raw) => decode_lock(&raw).map(Some),
            None => Ok(None),
        }
    }

    async fn list(&self, repo: &str, after: Option<&str>, limit: usize) -> GitResult<Vec<Lock>> {
        let after = after.map(|a| lock_key(repo, a));
        let rows = self
            .kv
            .scan(NS_LOCK, &lock_key(repo, ""), after.as_deref(), limit)
            .await?;
        rows.iter().map(|(_, v)| decode_lock(v)).collect()
    }

    async fn remove(&self, repo: &str, lock: &Lock) -> GitResult<bool> {
        let by_path = lock_key(repo, &lock.path);
        let Some(current) = self.kv.get(NS_LOCK, &by_path).await? else {
            return Ok(false);
        };
        if decode_lock(&current)?.id != lock.id {
            return Ok(false);
        }
        Ok(self
            .kv
            .apply(vec![
                KvOp::expect(NS_LOCK, by_path.clone(), Some(current)),
                KvOp::delete(NS_LOCK, by_path),
                KvOp::delete(NS_LOCK_ID, lock_key(repo, &lock.id)),
            ])
            .await?)
    }
}
