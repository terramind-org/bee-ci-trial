//! `ObjectService/GetDaemonSnapshot` (PRODUCT §4.5, `docs/spec/cache-tiers.md` §6): a verified
//! bundle of every object a node daemon needs to start warm for one repo scope.
//!
//! The snapshot covers the readable blobs of `main`'s tip under the requested sparse prefixes,
//! as the caller may read them (restricted paths are left out, redacted objects skipped). It
//! is encoded as `SWSN` v1 (below), stored content-addressed in the blob store and handed out
//! by signed URL (SPEC §10.5), so it needs signed URLs enabled. Unreferenced snapshot bytes
//! are orphans to the blob store and age out with GC's upload window.
//!
//! `SWSN` v1: `"SWSN"`, version `u32` LE, object count `u64` LE, then per object its 34-byte
//! id, length `u64` LE and bytes (ids ascending), then a 32-byte BLAKE3 of everything before.
//! `DaemonSnapshot.digest` is BLAKE3 of the whole file; daemons verify it, the trailer and
//! every object against its id before using any of it.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bee_core::{Mode, ObjectId, RefName, RepoPath};
use bee_policy::{Action, Policy};
use bee_proto::v1::{self, ErrorReason};
use bee_store::{ReadFrom, StorageClass};

use crate::auth::Caller;
use crate::convert::id_to;
use crate::errors::{ApiError, ApiResult};
use crate::state::{AppState, RepoCtx};

/// Magic of a snapshot file.
pub const MAGIC: &[u8; 4] = b"SWSN";
/// Format version.
pub const VERSION: u32 = 1;
/// Largest snapshot built (sum of object bytes).
pub const MAX_SNAPSHOT_BYTES: u64 = 1 << 30;
/// The ref a snapshot follows.
pub const SNAPSHOT_REF: &str = "main";

/// Builds the caller's snapshot of a repo scope and returns where to download it.
pub async fn daemon_snapshot(
    s: &AppState,
    caller: &Caller,
    req: v1::GetDaemonSnapshotRequest,
) -> ApiResult<(RepoCtx, Arc<Policy>, v1::DaemonSnapshot)> {
    let repo = s.repo(req.repo.as_ref()).await?;
    let policy = s
        .gate(caller, &repo, Action::Read, Some(SNAPSHOT_REF))
        .await?;
    if s.url_issuer.is_none() {
        return Err(ApiError::new(
            ErrorReason::Unimplemented,
            "daemon snapshots are delivered by signed URL, which this server has turned off",
        ));
    };
    let name = RefName::new(SNAPSHOT_REF.to_string()).map_err(ApiError::from)?;
    let current = s
        .meta
        .get_ref(repo.repo.id, &name)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("ref {SNAPSHOT_REF} not found")))?;
    let tip = current
        .target
        .ok_or_else(|| ApiError::not_found(format!("ref {SNAPSHOT_REF} is deleted")))?;
    let mut prefixes: Vec<Option<RepoPath>> = Vec::new();
    for raw in &req.sparse_prefixes {
        let trimmed: Vec<u8> = {
            let t = raw.as_slice();
            let start = t.iter().position(|&b| b != b'/').unwrap_or(t.len());
            let end = t.iter().rposition(|&b| b != b'/').map_or(start, |i| i + 1);
            t[start..end.max(start)].to_vec()
        };
        if trimmed.is_empty() {
            prefixes = vec![None];
            break;
        }
        prefixes.push(Some(RepoPath::new(trimmed).map_err(ApiError::from)?));
    }
    if prefixes.is_empty() {
        prefixes.push(None);
    }
    let history = s.history(&repo);
    let mut ids: BTreeSet<ObjectId> = BTreeSet::new();
    for prefix in &prefixes {
        if let Some(p) = prefix
            && !s.path_allowed(caller, repo.repo.id, &policy, Action::Read, &p.to_string())
        {
            continue;
        }
        let mut token: Option<String> = None;
        let mut restricted_dirs: Vec<RepoPath> = Vec::new();
        loop {
            let page = match history
                .list_tree(&tip, prefix.as_ref(), true, 1000, token.as_deref())
                .await
            {
                Ok(p) => p,
                // A prefix naming nothing (or a file) contributes nothing.
                Err(e)
                    if prefix.is_some()
                        && matches!(
                            e.reason,
                            bee_core::Reason::NotFound | bee_core::Reason::InvalidArgument
                        ) =>
                {
                    break;
                }
                Err(e) => return Err(e.into()),
            };
            for item in page.items {
                if restricted_dirs.iter().any(|d| d.is_ancestor_of(&item.path)) {
                    continue;
                }
                if !s.path_allowed(
                    caller,
                    repo.repo.id,
                    &policy,
                    Action::Read,
                    &item.path.to_string(),
                ) {
                    if item.mode == Mode::Tree {
                        restricted_dirs.push(item.path.clone());
                    }
                    continue;
                }
                if matches!(item.mode, Mode::File | Mode::Executable | Mode::Symlink) {
                    ids.insert(item.id);
                }
            }
            match page.next_page_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
    }
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&VERSION.to_le_bytes());
    let count_at = body.len();
    body.extend_from_slice(&0u64.to_le_bytes());
    let mut count = 0u64;
    let mut total = 0u64;
    for id in &ids {
        if crate::redaction::is_redacted(s, repo.org.id, id).await? {
            continue;
        }
        let bytes = match bee_store::trees::read_object_bytes(
            s.meta.as_ref(),
            s.blobs.as_ref(),
            repo.org.id,
            id,
            ReadFrom::Replica(None),
        )
        .await
        {
            Ok(b) => b,
            Err(e)
                if matches!(
                    e.reason,
                    bee_core::Reason::Redacted | bee_core::Reason::MissingObject
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        total += bytes.len() as u64;
        if total > MAX_SNAPSHOT_BYTES {
            return Err(ApiError::new(
                ErrorReason::TooLarge,
                format!(
                    "the scope holds more than {MAX_SNAPSHOT_BYTES} bytes; narrow the sparse prefixes"
                ),
            ));
        }
        body.extend_from_slice(id.as_bytes());
        body.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        body.extend_from_slice(&bytes);
        count += 1;
    }
    body[count_at..count_at + 8].copy_from_slice(&count.to_le_bytes());
    let trailer = bee_object::digest(&body);
    body.extend_from_slice(&trailer);
    let digest = bee_object::digest(&body);
    let size = body.len() as u64;
    let file_id = bee_object::blob_id(&body);
    s.blobs
        .put(repo.org.id, &file_id, body, StorageClass::Hot)
        .await?;
    let ttl = Duration::from_secs(s.cfg.blobs.signed_url_ttl_secs.max(1));
    let url = s
        .issue_url(repo.org.id, &file_id, ttl, Some(caller), &[&repo])
        .await?;
    let (_, redaction_epoch) = s.epochs(&repo, &policy).await?;
    Ok((
        repo,
        policy,
        v1::DaemonSnapshot {
            id: file_id.to_text(),
            root_changeset: Some(id_to(&tip)),
            r#ref: SNAPSHOT_REF.into(),
            ref_version: current.version,
            digest: digest.to_vec(),
            size_bytes: size,
            signed_url: url.url,
            signed_url_expires_unix: (url.expires_at_ms / 1000) as i64,
            created_at: Some(crate::util::timestamp_ms(s.now_ms())),
            redaction_epoch,
        },
    ))
}
