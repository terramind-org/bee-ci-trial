//! Repo mirrors (F416-031): the sync worker behind `RepoService.SyncRepoMirror` and the
//! schedule (`interval_seconds`, at least 300).
//!
//! - **Pull** mirrors fetch the remote's branches and tags (or the `ref_patterns`: `main`,
//!   `release/*`, `tags/v*`) into a bare cache at `<data_dir>/mirrors/<mirror id>.git` with the
//!   `git` client, then import them with `bee-import` (exact git ids; refs move to the
//!   remote's tips, ref log principal `mirror:<id>`). Refs deleted upstream are kept.
//! - **Push** mirrors push the repo's branches and tags with `git push --mirror` (the bare
//!   repository `ExportService` builds).
//!
//! The credential is sent as HTTP Basic `x-access-token:<credential>` for `https://` and
//! `http://` remotes and never logged. Every sync records `last_sync_at` and `last_error`
//! (empty on success) on the mirror and emits `mirror.synced`. The worker polls every
//! `forge.mirror_poll_ms` and runs mirrors that never synced or whose interval elapsed, one at
//! a time; a sync that fails is retried at its next interval.

use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use bee_forge::orgs::MirrorSyncer;
use bee_forge::{Cx, ForgeError};
use bee_proto::v1::{self, ErrorReason, MirrorDirection, Repo, RepoMirror};

use super::ForgeWiring;
use super::adapters::repo_ctx;
use crate::errors::{ApiError, ApiResult};
use crate::state::{AppState, RepoCtx};

type FResult<T> = bee_forge::Result<T>;

/// The mirror worker over the server's stores.
pub struct ServerMirrors {
    state: Weak<AppState>,
}

impl ServerMirrors {
    /// A worker over `state`.
    pub fn new(state: &Arc<AppState>) -> ServerMirrors {
        ServerMirrors {
            state: Arc::downgrade(state),
        }
    }
}

fn refspecs(patterns: &[String]) -> Vec<String> {
    if patterns.is_empty() {
        return vec![
            "+refs/heads/*:refs/heads/*".into(),
            "+refs/tags/*:refs/tags/*".into(),
        ];
    }
    patterns
        .iter()
        .map(|p| {
            let p = p.trim_start_matches("refs/");
            let full = if let Some(t) = p.strip_prefix("tags/") {
                format!("refs/tags/{t}")
            } else {
                format!("refs/heads/{}", p.trim_start_matches("heads/"))
            };
            format!("+{full}:{full}")
        })
        .collect()
}

async fn git(dir: &Path, auth: Option<&str>, args: &[String], secret: &str) -> ApiResult<()> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir").arg(dir);
    if let Some(a) = auth {
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: {a}"));
    }
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(1800), cmd.output())
        .await
        .map_err(|_| ApiError::new(ErrorReason::DeadlineExceeded, "git timed out"))?
        .map_err(|e| ApiError::new(ErrorReason::Unavailable, format!("running git: {e}")))?;
    if !out.status.success() {
        let mut err = String::from_utf8_lossy(&out.stderr).to_string();
        if !secret.is_empty() {
            err = err.replace(secret, "<redacted>");
        }
        return Err(ApiError::new(
            ErrorReason::Unavailable,
            format!(
                "git {} failed: {}",
                args.first().map_or("", String::as_str),
                err.trim()
            ),
        ));
    }
    Ok(())
}

fn basic(credential: &str, url: &str) -> Option<String> {
    (!credential.is_empty() && (url.starts_with("https://") || url.starts_with("http://"))).then(
        || {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD
                    .encode(format!("x-access-token:{credential}"))
            )
        },
    )
}

/// Fetches the remote into the mirror's bare cache and imports it.
async fn pull(state: &AppState, ctx: &RepoCtx, m: &RepoMirror, credential: &str) -> ApiResult<()> {
    let dir = state
        .cfg
        .server
        .data_dir
        .join("mirrors")
        .join(format!("{}.git", m.id));
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| ApiError::new(ErrorReason::Internal, format!("mirror cache: {e}")))?;
        git(
            &dir,
            None,
            &[
                "init".into(),
                "--bare".into(),
                "-q".into(),
                "--template=".into(),
            ],
            "",
        )
        .await?;
    }
    let mut args = vec![
        "fetch".to_string(),
        "-q".into(),
        "--no-auto-gc".into(),
        m.remote_url.clone(),
    ];
    args.extend(refspecs(&m.ref_patterns));
    git(
        &dir,
        basic(credential, &m.remote_url).as_deref(),
        &args,
        credential,
    )
    .await?;
    let target = bee_import::ImportTarget {
        writer: state.writer.clone(),
        org: ctx.org.id,
        repo: ctx.repo.id,
    };
    let mut opts = bee_import::ImportOptions::new(format!("mirror:{}", m.id));
    opts.at_ms = state.now_ms();
    opts.source = format!("mirror:{}", m.remote_url);
    opts.derived = Some(Arc::new(crate::services::import::EagerHook::new(
        state.registry.clone(),
    )));
    bee_import::import_repository(&dir, &target, &opts).await?;
    Ok(())
}

/// One lock per mirror: the schedule and `SyncRepoMirror` never run a mirror twice at once.
fn mirror_lock(id: &str) -> Arc<tokio::sync::Mutex<()>> {
    type Locks = std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>;
    static LOCKS: std::sync::OnceLock<Locks> = std::sync::OnceLock::new();
    let mut m = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    m.entry(id.to_string()).or_default().clone()
}

/// Runs one sync and records its outcome; the mirror comes back with its status.
pub async fn sync_once(
    state: &AppState,
    w: &ForgeWiring,
    repo_id: &str,
    id: &str,
) -> FResult<RepoMirror> {
    let lock = mirror_lock(id);
    let _held = lock.lock().await;
    let cx = Cx::system();
    let rec = w.forge.orgs.mirror_record(&cx, repo_id, id).await?;
    let m = rec.mirror.clone().unwrap_or_default();
    let ctx = repo_ctx(state, repo_id).await?;
    let outcome = match MirrorDirection::try_from(m.direction)
        .unwrap_or(MirrorDirection::Unspecified)
    {
        MirrorDirection::Pull => pull(state, &ctx, &m, &rec.credential).await,
        MirrorDirection::Push => match crate::services::export::bare_repo(state, &ctx).await {
            Ok(bare) => {
                crate::services::export::push(state, &bare, &m.remote_url, &rec.credential).await
            }
            Err(e) => Err(e),
        },
        MirrorDirection::Unspecified => Err(ApiError::invalid(
            "missing_field",
            "the mirror has no direction",
        )),
    };
    let error = match &outcome {
        Ok(()) => String::new(),
        Err(e) => e.detail.message.clone(),
    };
    w.forge
        .orgs
        .record_mirror_sync(&cx, repo_id, id, &error)
        .await
}

#[async_trait]
impl MirrorSyncer for ServerMirrors {
    async fn sync(&self, repo: &Repo, mirror_id: &str) -> FResult<RepoMirror> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| ForgeError::unavailable("server stopping"))?;
        let w = state
            .hub
            .get()
            .cloned()
            .ok_or_else(|| ForgeError::unavailable("forge not open yet"))?;
        sync_once(&state, &w, &repo.id, mirror_id).await
    }
}

/// Whether a mirror is due at `now_ms`.
fn due(m: &v1::RepoMirror, now_ms: u64) -> bool {
    match &m.last_sync_at {
        None => true,
        Some(t) => {
            let last = (t.seconds.max(0) as u64) * 1000 + (t.nanos.max(0) as u64) / 1_000_000;
            now_ms >= last + u64::from(m.interval_seconds.max(300)) * 1000
        }
    }
}

impl ForgeWiring {
    /// One pass of the mirror schedule: syncs every mirror that is due.
    pub(crate) async fn sync_due_mirrors(&self) {
        let Some(state) = self.state() else { return };
        let Ok(all) = self.forge.orgs.all_mirrors(&Cx::system()).await else {
            return;
        };
        for m in all {
            if due(&m, state.now_ms()) {
                let _ = sync_once(&state, self, &m.repo_id, &m.id).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_become_refspecs() {
        assert_eq!(refspecs(&[]).len(), 2);
        assert_eq!(
            refspecs(&["main".into(), "release/*".into(), "tags/v*".into()]),
            vec![
                "+refs/heads/main:refs/heads/main",
                "+refs/heads/release/*:refs/heads/release/*",
                "+refs/tags/v*:refs/tags/v*"
            ]
        );
    }

    #[test]
    fn schedule() {
        let mut m = v1::RepoMirror {
            interval_seconds: 300,
            ..Default::default()
        };
        assert!(due(&m, 0));
        m.last_sync_at = Some(pbjson_types::Timestamp {
            seconds: 1000,
            nanos: 0,
        });
        assert!(!due(&m, 1_000_000 + 299_000));
        assert!(due(&m, 1_000_000 + 300_000));
    }
}
