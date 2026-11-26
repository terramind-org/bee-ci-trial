//! Git over SSH on this server (`bee_git_http::ssh`, PLAN W16): the listener, the host key and
//! the registry of the SSH keys users authenticate with.
//!
//! The transport runs the gateway's engine, so a push over SSH meets the same repo gate,
//! policy, push protection, Land coalescer and audit as one over HTTP. What this module adds is
//! the server side of identity: a key maps to a principal, and an authenticated principal gets
//! a short-lived token minted for the session (see `git_mount`).
//!
//! # Key registry
//!
//! The identity domain's `UserService.AddUserSshKey` is served elsewhere and has no key store, so
//! this module keeps a minimal one in the server's key-value tables: [`add_key`], [`list_keys`]
//! and [`remove_key`]. A key is registered to one principal (a human or service; agents hold no
//! SSH keys, per `users.proto`) and is unique across the server by its `SHA256:` fingerprint.
//! When the identity domain gains a store, its RPCs call these functions (or replace them
//! behind `GitBackend::ssh_principal`).
//!
//! # Configuration
//!
//! The listener starts with the server when `BEE_GIT_SSH_LISTEN` names an address
//! (`0.0.0.0:2222`; port 0 picks one and logs it). The host key is read from
//! `BEE_GIT_SSH_HOST_KEY` (an OpenSSH private key file), or
//! `<data_dir>/ssh_host_ed25519_key`, which is generated with mode 0600 on first start so the
//! server's identity survives restarts. Tests and embedders call [`start`] directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bee_git_http::GitGateway;
use bee_git_http::ssh::{self, PrivateKey, SshConfig, SshIdentity, SshServer};
use bee_proto::v1::ErrorReason;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::errors::{ApiError, ApiResult};
use crate::kv::KvOp;
use crate::logging::{self, Level};
use crate::state::AppState;

/// Namespace of registered SSH keys: fingerprint → [`SshKeyRecord`].
pub const NS_SSH_KEY: &str = "ssh_key";
/// Namespace of a principal's keys: `<principal>\0<fingerprint>` → empty.
pub const NS_SSH_KEY_USER: &str = "ssh_key_user";

/// A registered SSH key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKeyRecord {
    /// The principal the key authenticates as.
    pub principal: String,
    /// `SHA256:…`, the key's identity.
    pub fingerprint: String,
    /// The public key in OpenSSH format (`ssh-ed25519 AAAA…`), without a comment.
    pub public_key: String,
    /// The user's label for the key.
    pub title: String,
    /// When it was registered.
    pub created_ms: u64,
}

fn user_key(principal: &str, fingerprint: &str) -> Vec<u8> {
    let mut k = principal.as_bytes().to_vec();
    k.push(0);
    k.extend_from_slice(fingerprint.as_bytes());
    k
}

/// Registers an OpenSSH public key to a principal. `INVALID_ARGUMENT` for a key that does not
/// parse, `NOT_FOUND` for an unknown principal, `PERMISSION_DENIED` for an agent or a
/// deactivated principal, `ALREADY_EXISTS` when the key is registered (to anyone).
pub async fn add_key(
    state: &AppState,
    principal: &str,
    public_key: &str,
    title: &str,
) -> ApiResult<SshKeyRecord> {
    let key = ssh::parse_public_key(public_key)
        .map_err(|e| ApiError::invalid("bad_ssh_key", e.message))?;
    let rec = state
        .principal(principal)
        .await?
        .ok_or_else(|| ApiError::not_found("principal not found"))?;
    if rec.deactivated || rec.policy_kind() == bee_policy::PrincipalKind::Agent {
        return Err(ApiError::denied(
            "SSH keys belong to active humans and services, not agents or deactivated principals",
        ));
    }
    let fingerprint = ssh::fingerprint(&key);
    // The stored line has no comment: the title is the label.
    let line = key
        .to_openssh()
        .map_err(|e| ApiError::invalid("bad_ssh_key", format!("cannot encode the key: {e}")))?;
    let record = SshKeyRecord {
        principal: principal.to_string(),
        fingerprint: fingerprint.clone(),
        public_key: line
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" "),
        title: title.to_string(),
        created_ms: state.now_ms(),
    };
    let row =
        serde_json::to_vec(&record).map_err(|e| ApiError::invalid("encode", e.to_string()))?;
    let won = state
        .kv
        .apply(vec![
            KvOp::expect(NS_SSH_KEY, fingerprint.as_bytes().to_vec(), None),
            KvOp::put(NS_SSH_KEY, fingerprint.as_bytes().to_vec(), row),
            KvOp::put(
                NS_SSH_KEY_USER,
                user_key(principal, &fingerprint),
                Vec::new(),
            ),
        ])
        .await?;
    if !won {
        return Err(ApiError::new(
            ErrorReason::AlreadyExists,
            "this SSH key is already registered",
        ));
    }
    Ok(record)
}

/// A registered key by fingerprint.
pub async fn get_key(state: &AppState, fingerprint: &str) -> ApiResult<Option<SshKeyRecord>> {
    Ok(state
        .kv
        .get(NS_SSH_KEY, fingerprint.as_bytes())
        .await?
        .and_then(|b| serde_json::from_slice(&b).ok()))
}

/// A principal's keys, ordered by fingerprint.
pub async fn list_keys(state: &AppState, principal: &str) -> ApiResult<Vec<SshKeyRecord>> {
    let mut prefix = principal.as_bytes().to_vec();
    prefix.push(0);
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = state
            .kv
            .scan(NS_SSH_KEY_USER, &prefix, after.as_deref(), 200)
            .await?;
        let Some(last) = page.last() else { break };
        after = Some(last.0.clone());
        for (k, _) in &page {
            let fp = String::from_utf8_lossy(&k[prefix.len()..]).into_owned();
            if let Some(r) = get_key(state, &fp).await? {
                out.push(r);
            }
        }
    }
    Ok(out)
}

/// Removes one of the principal's keys; false when it is not theirs or not registered.
pub async fn remove_key(state: &AppState, principal: &str, fingerprint: &str) -> ApiResult<bool> {
    let Some(raw) = state.kv.get(NS_SSH_KEY, fingerprint.as_bytes()).await? else {
        return Ok(false);
    };
    let Ok(rec) = serde_json::from_slice::<SshKeyRecord>(&raw) else {
        return Ok(false);
    };
    if rec.principal != principal {
        return Ok(false);
    }
    Ok(state
        .kv
        .apply(vec![
            KvOp::expect(NS_SSH_KEY, fingerprint.as_bytes().to_vec(), Some(raw)),
            KvOp::delete(NS_SSH_KEY, fingerprint.as_bytes().to_vec()),
            KvOp::delete(NS_SSH_KEY_USER, user_key(principal, fingerprint)),
        ])
        .await?)
}

/// Who a key authenticates as: its principal, if that principal exists and is not
/// deactivated.
pub async fn identity_of(state: &AppState, fingerprint: &str) -> ApiResult<Option<SshIdentity>> {
    let Some(key) = get_key(state, fingerprint).await? else {
        return Ok(None);
    };
    let Some(rec) = state.principal(&key.principal).await? else {
        return Ok(None);
    };
    if rec.deactivated {
        return Ok(None);
    }
    let name = if rec.display.is_empty() {
        rec.id.clone()
    } else {
        rec.display.clone()
    };
    Ok(Some(SshIdentity {
        principal: rec.id,
        name,
    }))
}

fn host_key_error(path: &Path, what: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        ErrorReason::Internal,
        format!("SSH host key {}: {what}", path.display()),
    )
}

/// The host key at `path`, generating and saving one (mode 0600) when the file is absent.
pub fn load_or_create_host_key(path: &Path) -> ApiResult<PrivateKey> {
    match std::fs::read_to_string(path) {
        Ok(text) => ssh::host_key_from_openssh(&text).map_err(|e| host_key_error(path, e.message)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = ssh::generate_host_key().map_err(|e| host_key_error(path, e.message))?;
            let text =
                ssh::host_key_to_openssh(&key).map_err(|e| host_key_error(path, e.message))?;
            write_private(path, text.as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(host_key_error(path, e)),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> ApiResult<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| host_key_error(path, e))?;
    f.write_all(bytes).map_err(|e| host_key_error(path, e))
}

/// Binds `addr` and serves git over SSH with `host_key` through `gateway`.
pub async fn start(
    gateway: &Arc<GitGateway>,
    addr: &str,
    host_key: PrivateKey,
) -> ApiResult<SshServer> {
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        ApiError::new(
            ErrorReason::Unavailable,
            format!("cannot listen for SSH on {addr}: {e}"),
        )
    })?;
    gateway
        .serve_ssh(listener, SshConfig::new(host_key))
        .await
        .map_err(|e| ApiError::new(ErrorReason::Internal, format!("SSH listener: {e}")))
}

/// Starts the listener when `BEE_GIT_SSH_LISTEN` is set; the returned task owns it (abort it
/// to stop serving).
pub async fn spawn_from_env(
    state: &AppState,
    gateway: &Arc<GitGateway>,
) -> ApiResult<Option<JoinHandle<()>>> {
    let Ok(listen) = std::env::var("BEE_GIT_SSH_LISTEN") else {
        return Ok(None);
    };
    if listen.trim().is_empty() {
        return Ok(None);
    }
    let key_path = std::env::var_os("BEE_GIT_SSH_HOST_KEY")
        .map(PathBuf::from)
        .unwrap_or_else(|| state.cfg.server.data_dir.join("ssh_host_ed25519_key"));
    let key = load_or_create_host_key(&key_path)?;
    let server = start(gateway, listen.trim(), key).await?;
    logging::log(
        Level::Info,
        "git-ssh",
        "listening",
        &[("addr", server.addr.to_string().into())],
    );
    Ok(Some(tokio::spawn(async move {
        let _server = server;
        std::future::pending::<()>().await;
    })))
}
