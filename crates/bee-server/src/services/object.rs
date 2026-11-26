//! `ObjectService` (SPEC §10.1, §10.5, §9.3): server info, verified object streams, existence
//! checks that cannot be used as probes, and strongly consistent ref resolution with
//! read-your-writes.

use std::sync::Arc;
use std::time::Duration;

use bee_core::{ObjectId, ObjectKind, RefName};
use bee_policy::{Action, Actor, Policy, PrincipalKind};
use bee_proto::v1::{self, object_service_server::ObjectService};
use bee_store::{ObjectState, ReadFrom};

use crate::auth::Caller;
use crate::convert::{id_from, id_to, ref_to};
use crate::errors::{ApiError, ApiResult};
use crate::services::{Rpc, RpcStream, stream_of};
use crate::state::{AppState, RepoCtx, RequestMeta};

/// Bytes per streamed chunk.
pub const CHUNK: usize = 1 << 20;
/// Objects at least this large are offered by signed URL when the client allows it.
pub const SIGNED_URL_MIN_BYTES: u64 = 64 * 1024;

/// The object service.
pub struct Objects {
    state: Arc<AppState>,
}

impl Objects {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> Objects {
        Objects { state }
    }
}

/// Maps a handler result to a tonic result, stamping the request id on errors.
pub fn finish<T>(meta: &RequestMeta, r: ApiResult<T>) -> Result<T, tonic::Status> {
    r.map_err(|e| e.with_request_id(&meta.request_id).into_status())
}

/// The access class of an object (SPEC §10.2): `ROOT` when every path it appears at is
/// readable by any repo reader, so caches may serve it after one positive repo-read decision.
pub async fn access_class(
    state: &AppState,
    repo: &RepoCtx,
    policy: &Policy,
    id: &ObjectId,
) -> ApiResult<i32> {
    if id.kind() == ObjectKind::Changeset || id.kind() == ObjectKind::Meta {
        return Ok(v1::AccessClass::Restricted as i32);
    }
    let paths = state
        .meta
        .object_paths(repo.repo.id, id, ReadFrom::Primary)
        .await?;
    let anyone = Actor::new("anyone", PrincipalKind::Human);
    let open = !paths.is_empty()
        && paths.iter().all(|p| {
            let path = p.as_ref().map(ToString::to_string).unwrap_or_default();
            policy.check_access(&anyone, Action::Read, &path).allowed
        });
    Ok(if open {
        v1::AccessClass::Root as i32
    } else {
        v1::AccessClass::Restricted as i32
    })
}

impl Objects {
    async fn server_info(&self) -> ApiResult<v1::ServerInfo> {
        let s = &self.state;
        let keys = s
            .key_infos()
            .into_iter()
            .filter(|k| k.retired_ms == 0)
            .map(|k| v1::TokenPublicKey {
                key_id: k.id.to_string(),
                algorithm: "ed25519".into(),
                public_key: k.public_key.clone(),
                not_after: None,
            })
            .collect();
        let mut features = vec![
            "grpc",
            "connect_json",
            "mcp",
            "land_coalescer",
            "min_version",
        ];
        if s.url_issuer.is_some() {
            features.push("signed_urls");
        }
        Ok(v1::ServerInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            tier: "server".into(),
            algorithms: vec!["blake3-256".into(), "scbor-v1".into(), "diff3-v1".into()],
            max_blob_bytes: s.cfg.limits.max_blob_bytes,
            protocol_versions: vec!["bee.v1".into()],
            policy_epoch: "0".into(),
            redaction_epoch: 0,
            token_public_keys: keys,
            limits: Some(v1::ServerLimits {
                max_path_bytes: 4096,
                max_path_components: 256,
                max_name_bytes: 255,
                max_tree_entries: 1_000_000,
                max_batch_ids: s.cfg.limits.max_batch_ids,
                inline_object_bytes: bee_store::INLINE_MAX_BYTES as u64,
                max_delta_entries: 1_000_000,
            }),
            github_client_id: s.cfg.login.github_client_id.clone(),
            cell: s.cfg.server.cell.clone(),
            region: s.cfg.server.region.clone(),
            features: features.into_iter().map(String::from).collect(),
            connect_base_url: s.public_url(),
            edition: s.cfg.server.edition.as_str().into(),
        })
    }

    async fn readable_ctx(&self, caller: &Caller, repo: &RepoCtx) -> ApiResult<Arc<Policy>> {
        self.state.gate(caller, repo, Action::Read, None).await
    }

    async fn object_chunks(
        &self,
        caller: &Caller,
        req: v1::GetObjectsRequest,
    ) -> ApiResult<(
        RepoCtx,
        Arc<Policy>,
        Vec<Result<v1::ObjectChunk, tonic::Status>>,
    )> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let policy = self.readable_ctx(caller, &repo).await?;
        if req.ids.len() > s.cfg.limits.max_batch_ids as usize {
            return Err(ApiError::invalid(
                "too_many_ids",
                format!("at most {} ids per request", s.cfg.limits.max_batch_ids),
            ));
        }
        // The request's token was taken on authentication; bulk reads pay for their ids.
        s.charge_rate(caller, crate::auth::bulk_read_cost(req.ids.len()) - 1)?;
        let ids = req
            .ids
            .iter()
            .map(|raw| id_from(Some(raw), "ids"))
            .collect::<Result<Vec<ObjectId>, _>>()?;
        // Read for the whole request at once: redactions, object rows (blobs and trees), and
        // the policy's view of the caller; per object only its paths and blob bytes remain.
        let redacted = crate::redaction::redacted_among(s, repo.org.id, &ids).await?;
        let stored: Vec<ObjectId> = ids
            .iter()
            .filter(|id| id.kind() != ObjectKind::Changeset)
            .copied()
            .collect();
        let rows: std::collections::HashMap<ObjectId, bee_store::ObjectRow> = s
            .meta
            .get_objects(repo.org.id, &stored, ReadFrom::Replica(None))
            .await?
            .into_iter()
            .flatten()
            .map(|r| (r.id, r))
            .collect();
        let actors = s.known_policy_actors(caller, repo.repo.id);
        let allowed = s.path_filter(caller, &actors, &policy, Action::Read);
        let anyone = [Actor::new("anyone", PrincipalKind::Human)];
        let open_to_anyone = policy.access_filter(&anyone, Action::Read);
        let mut out = Vec::new();
        for id in ids {
            let status_chunk = |status: v1::ItemStatus, reason: &str| v1::ObjectChunk {
                id: Some(id_to(&id)),
                last: true,
                status: status as i32,
                error_reason: reason.to_string(),
                ..Default::default()
            };
            // Readability and the access class from one read of the object's paths
            // (as `AppState::object_readable` and `access_class` decide them).
            let (readable, class) = match id.kind() {
                ObjectKind::Blob | ObjectKind::Tree => {
                    let paths = s
                        .meta
                        .object_paths(repo.repo.id, &id, ReadFrom::Primary)
                        .await?;
                    let readable = if paths.is_empty() {
                        s.object_readable(caller, &repo, &policy, &id).await?
                    } else {
                        paths
                            .iter()
                            .any(|p| p.as_ref().is_none_or(|p| allowed(&p.to_string())))
                    };
                    let open = !paths.is_empty()
                        && paths.iter().all(|p| {
                            open_to_anyone
                                .allows(&p.as_ref().map(ToString::to_string).unwrap_or_default())
                        });
                    let class = if open {
                        v1::AccessClass::Root as i32
                    } else {
                        v1::AccessClass::Restricted as i32
                    };
                    (readable, class)
                }
                _ => (
                    s.object_readable(caller, &repo, &policy, &id).await?,
                    access_class(s, &repo, &policy, &id).await?,
                ),
            };
            if !readable {
                out.push(Ok(status_chunk(v1::ItemStatus::NotFound, "NOT_FOUND")));
                continue;
            }
            if redacted.contains(&id) {
                out.push(Ok(status_chunk(v1::ItemStatus::Redacted, "REDACTED")));
                continue;
            }
            // Signed URL for large blob-store objects (SPEC §10.5), same check as a read.
            if req.allow_signed_urls
                && s.url_issuer.is_some()
                && id.kind() != ObjectKind::Changeset
                && let Some(row) = s
                    .meta
                    .get_object(repo.org.id, &id, ReadFrom::Primary)
                    .await?
                && row.state == ObjectState::Live
                && row.inline.is_none()
                && row.size >= SIGNED_URL_MIN_BYTES
            {
                let ttl = Duration::from_secs(s.cfg.blobs.signed_url_ttl_secs);
                if let Ok(url) = s
                    .issue_url(repo.org.id, &id, ttl, Some(caller), &[&repo])
                    .await
                {
                    out.push(Ok(v1::ObjectChunk {
                        id: Some(id_to(&id)),
                        last: true,
                        signed_url: url.url,
                        signed_url_expires_unix: (url.expires_at_ms / 1000) as i64,
                        status: v1::ItemStatus::Ok as i32,
                        access_class: class,
                        encoding: v1::ObjectEncoding::Raw as i32,
                        total_size: row.size,
                        ..Default::default()
                    }));
                    continue;
                }
            }
            let read = if id.kind() == ObjectKind::Changeset {
                match s
                    .meta
                    .get_changeset(repo.repo.id, &id, ReadFrom::Primary)
                    .await?
                {
                    Some(row) => Ok(row.bytes),
                    None => Err(bee_core::Reason::MissingObject),
                }
            } else {
                match rows.get(&id) {
                    // The row read above, checked as `read_object_bytes` checks it.
                    Some(row) => match row.state {
                        ObjectState::Purged => Err(bee_core::Reason::Redacted),
                        ObjectState::Quarantined => Err(bee_core::Reason::MissingObject),
                        ObjectState::Live => {
                            let bytes = match &row.inline {
                                Some(b) => Some(b.clone()),
                                None => s.blobs.get(repo.org.id, &id).await?,
                            };
                            match bytes {
                                Some(b) if bee_object::hash_id(id.kind(), &b) == id => Ok(b),
                                Some(_) => {
                                    return Err(bee_core::BeeError::new(
                                        bee_core::Reason::Integrity,
                                        format!("stored bytes do not hash to {id}"),
                                    )
                                    .into());
                                }
                                None => Err(bee_core::Reason::MissingObject),
                            }
                        }
                    },
                    // No row on the replica: the full read falls back to the primary.
                    None => match bee_store::trees::read_object_bytes(
                        s.meta.as_ref(),
                        s.blobs.as_ref(),
                        repo.org.id,
                        &id,
                        ReadFrom::Replica(None),
                    )
                    .await
                    {
                        Ok(b) => Ok(b),
                        Err(e) if e.reason == bee_core::Reason::Redacted => {
                            Err(bee_core::Reason::Redacted)
                        }
                        Err(e) if e.reason == bee_core::Reason::MissingObject => {
                            Err(bee_core::Reason::MissingObject)
                        }
                        Err(e) => return Err(e.into()),
                    },
                }
            };
            let bytes = match read {
                Ok(b) => b,
                Err(bee_core::Reason::Redacted) => {
                    out.push(Ok(status_chunk(v1::ItemStatus::Redacted, "REDACTED")));
                    continue;
                }
                Err(_) => {
                    out.push(Ok(status_chunk(v1::ItemStatus::NotFound, "NOT_FOUND")));
                    continue;
                }
            };
            let total = bytes.len() as u64;
            let start = req.resume_offset.min(total) as usize;
            let end = if req.length > 0 {
                (start as u64 + req.length).min(total) as usize
            } else {
                total as usize
            };
            let mut pos = start;
            loop {
                let next = (pos + CHUNK).min(end);
                out.push(Ok(v1::ObjectChunk {
                    id: Some(id_to(&id)),
                    offset: pos as u64,
                    data: bytes[pos..next].to_vec(),
                    last: next >= end,
                    status: v1::ItemStatus::Ok as i32,
                    access_class: class,
                    encoding: v1::ObjectEncoding::Raw as i32,
                    total_size: total,
                    ..Default::default()
                }));
                pos = next;
                if pos >= end {
                    break;
                }
            }
        }
        drop(allowed);
        drop(open_to_anyone);
        Ok((repo, policy, out))
    }

    async fn has(
        &self,
        caller: &Caller,
        req: v1::HasObjectsRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::HasObjectsResponse)> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let policy = self.readable_ctx(caller, &repo).await?;
        if req.ids.len() > s.cfg.limits.max_batch_ids as usize {
            return Err(ApiError::invalid("too_many_ids", "too many ids"));
        }
        s.charge_rate(caller, crate::auth::bulk_read_cost(req.ids.len()) - 1)?;
        let mut present = Vec::with_capacity(req.ids.len());
        for raw in &req.ids {
            let id = id_from(Some(raw), "ids")?;
            let readable = s.object_readable(caller, &repo, &policy, &id).await?;
            let exists = readable
                && match id.kind() {
                    ObjectKind::Changeset => true,
                    _ => s
                        .meta
                        .get_object(repo.org.id, &id, ReadFrom::Primary)
                        .await?
                        .is_some_and(|r| r.state == ObjectState::Live),
                };
            present.push(exists);
        }
        Ok((repo, policy, v1::HasObjectsResponse { present }))
    }

    async fn resolve(
        &self,
        caller: &Caller,
        req: v1::ResolveRefRequest,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::ResolveRefResponse)> {
        self.resolve_hops(caller, req, 0).await
    }

    async fn resolve_hops(
        &self,
        caller: &Caller,
        req: v1::ResolveRefRequest,
        hops: u8,
    ) -> ApiResult<(RepoCtx, Arc<Policy>, v1::ResolveRefResponse)> {
        let s = &self.state;
        let repo = s.repo(req.repo.as_ref()).await?;
        let name = RefName::new(req.name.clone()).map_err(ApiError::from)?;
        let policy = s
            .gate(caller, &repo, Action::Read, Some(name.as_str()))
            .await?;
        let (mut current, stale) = s.read_ref(repo.repo.id, &name).await?;
        if stale
            && current
                .as_ref()
                .is_some_and(|r| r.version < req.min_version)
        {
            // The last known version is older than one the client saw: no stale answer.
            return Err(ApiError::new(
                v1::ErrorReason::Unavailable,
                format!(
                    "metadata primary unavailable; the last known version of {} is older than \
                     min_version {}",
                    name.as_str(),
                    req.min_version
                ),
            ));
        }
        // Read-your-writes: refs are read from the primary, which is always at or past any
        // version a client saw; a lagging read waits up to read_wait_ms.
        if !stale
            && req.min_version > 0
            && current
                .as_ref()
                .is_some_and(|r| r.version < req.min_version)
        {
            let until =
                std::time::Instant::now() + Duration::from_millis(s.cfg.metadata.read_wait_ms);
            while std::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(5)).await;
                current = s.meta.get_ref(repo.repo.id, &name).await?;
                if current
                    .as_ref()
                    .is_some_and(|r| r.version >= req.min_version)
                {
                    break;
                }
            }
        }
        // A renamed ref: the old name redirects to the new one while no ref of that name
        // exists (at most a few hops).
        if req.at_version == 0
            && hops < 4
            && !current.as_ref().is_some_and(|r| r.target.is_some())
            && let Some(to) = crate::services::refs::renamed_to(s, &repo, name.as_str()).await?
        {
            let mut next = req.clone();
            next.name = to.clone();
            next.min_version = 0;
            let (repo, policy, mut resp) =
                Box::pin(self.resolve_hops(caller, next, hops + 1)).await?;
            resp.renamed_to = to;
            return Ok((repo, policy, resp));
        }
        let Some(cur) = current else {
            return Err(ApiError::not_found(format!(
                "ref {} not found",
                name.as_str()
            )));
        };
        let (target, version) = if req.at_version > 0 {
            if req.at_version > cur.version {
                return Err(ApiError::not_found(format!(
                    "{}@{} does not exist yet",
                    name.as_str(),
                    req.at_version
                )));
            }
            let entry = s
                .meta
                .ref_log(repo.repo.id, &name, req.at_version, 1)
                .await?
                .into_iter()
                .next()
                .filter(|e| e.version == req.at_version)
                .ok_or_else(|| ApiError::not_found("ref log entry not found"))?;
            (entry.new, entry.version)
        } else {
            (cur.target, cur.version)
        };
        let Some(target) = target else {
            return Err(ApiError::not_found(format!(
                "ref {} is deleted",
                name.as_str()
            )));
        };
        let root = match s
            .meta
            .get_changeset(repo.repo.id, &target, ReadFrom::Primary)
            .await
        {
            Ok(c) => c.map(|c| *c.changeset.root()),
            // Stale: the root remembered with the ref (changesets are immutable).
            Err(_) if stale => s
                .last_refs
                .stale(repo.repo.id, &name, s.now_ms(), u64::MAX)
                .filter(|(r, _)| r.target == Some(target))
                .and_then(|(_, root)| root),
            Err(e) => return Err(e.into()),
        };
        if !stale && req.at_version == 0 {
            s.last_refs.record(repo.repo.id, &cur, root, s.now_ms());
        }
        Ok((
            repo,
            policy,
            v1::ResolveRefResponse {
                r#ref: Some(ref_to(name.as_str(), Some(&target), version)),
                root_tree: root.as_ref().map(id_to),
                stale,
                renamed_to: String::new(),
            },
        ))
    }
}

#[tonic::async_trait]
impl ObjectService for Objects {
    async fn get_server_info(
        &self,
        req: tonic::Request<v1::GetServerInfoRequest>,
    ) -> Rpc<v1::ServerInfo> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let mut info = self.server_info().await?;
            let Some(repo_ref) = req.get_ref().repo.clone() else {
                return Ok(tonic::Response::new(info));
            };
            // Epochs are per repo (SPEC §9.5): naming one needs a caller who can read it.
            let caller = self.state.caller(req.metadata()).await?;
            let repo = self.state.repo(Some(&repo_ref)).await?;
            let policy = self.readable_ctx(&caller, &repo).await?;
            let (p, r) = self.state.epochs(&repo, &policy).await?;
            info.policy_epoch = p.to_string();
            info.redaction_epoch = r;
            self.state
                .with_epochs(&repo, &policy, tonic::Response::new(info))
                .await
        }
        .await;
        finish(&meta, r)
    }

    type GetObjectsStream = RpcStream<v1::ObjectChunk>;

    async fn get_objects(
        &self,
        req: tonic::Request<v1::GetObjectsRequest>,
    ) -> Rpc<Self::GetObjectsStream> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            let (repo, policy, chunks) = self.object_chunks(&caller, req.into_inner()).await?;
            self.state
                .with_epochs(&repo, &policy, tonic::Response::new(stream_of(chunks)))
                .await
        }
        .await;
        finish(&meta, r)
    }

    async fn has_objects(
        &self,
        req: tonic::Request<v1::HasObjectsRequest>,
    ) -> Rpc<v1::HasObjectsResponse> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            let (repo, policy, resp) = self.has(&caller, req.into_inner()).await?;
            self.state
                .with_epochs(&repo, &policy, tonic::Response::new(resp))
                .await
        }
        .await;
        finish(&meta, r)
    }

    async fn resolve_ref(
        &self,
        req: tonic::Request<v1::ResolveRefRequest>,
    ) -> Rpc<v1::ResolveRefResponse> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            let (repo, policy, resp) = self.resolve(&caller, req.into_inner()).await?;
            self.state
                .with_epochs(&repo, &policy, tonic::Response::new(resp))
                .await
        }
        .await;
        finish(&meta, r)
    }

    async fn get_tier_stats(
        &self,
        req: tonic::Request<v1::GetTierStatsRequest>,
    ) -> Rpc<v1::TierStats> {
        let meta = RequestMeta::of(&req);
        let r = async {
            self.state.caller(req.metadata()).await?;
            Ok(tonic::Response::new(v1::TierStats {
                tier: "server".into(),
                started_at: Some(crate::util::timestamp_ms(self.state.started_ms())),
                ..Default::default()
            }))
        }
        .await;
        finish(&meta, r)
    }

    async fn get_daemon_snapshot(
        &self,
        req: tonic::Request<v1::GetDaemonSnapshotRequest>,
    ) -> Rpc<v1::DaemonSnapshot> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            let (repo, policy, snap) =
                crate::services::daemon::daemon_snapshot(&self.state, &caller, req.into_inner())
                    .await?;
            self.state
                .with_epochs(&repo, &policy, tonic::Response::new(snap))
                .await
        }
        .await;
        finish(&meta, r)
    }
}
