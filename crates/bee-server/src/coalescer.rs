//! Land (SPEC §7.4) and the required land coalescer.
//!
//! **One fenced owner per protected ref.** Every protected ref has at most one owner task
//! across all server processes. Ownership is a lease row in the server store
//! (`lease/<repo>/<ref>` → owner, fence, expiry, endpoint); taking over an expired lease
//! increments the fence, so the fence is a fencing token. An owner renews at a third of the
//! lease time and checks its fence immediately before every publication; a deposed owner
//! stops publishing and hands its queue back for re-routing.
//!
//! **Queue, rebase, one CAS.** Lands for a protected ref are queued to its owner. The owner
//! takes every waiting land (up to `land.batch_limit`), and for each in arrival order:
//! computes the stack, consults the landed index (skip / `ALREADY_LANDED`), evaluates policy,
//! and rebases the stack onto the running tip (the previous stack's result). Every accepted
//! stack becomes one step of a single `MetaStore::update_ref`: K versions, K ref-log entries,
//! the landed rows and K `ref.updated` events in one transaction. Each stack's result is the
//! same as if it had landed alone at that position; conflicts and denials fail only their
//! own stack. A version mismatch or landed-index race retries the whole batch from a fresh
//! read, up to `land.attempt_budget`, then `LAND_CONTENTION`.
//!
//! **Routing.** A process that is not the owner forwards the land over gRPC to the owner's
//! advertised endpoint; if the owner cannot be reached it lands directly with the plain CAS
//! loop below (still correct: the ref CAS and the landed index make every path linearizable;
//! the coalescer is about throughput). Non-protected refs (`ws/*`, `green/*`, others) always
//! use the plain CAS loop.
//!
//! **Ref-log principal.** `RefUpdate` carries one principal for all K entries. When a batch
//! mixes principals, the ref log records `coalescer:<instance>` and the per-stack principal
//! chains are in each `ref.updated` event and in the audit log.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bee_core::{ChangeId, ObjectId, RefName};
use bee_history::{RebaseOptions, StackOutcome, StackStep};
use bee_object::DeltaOp;
use bee_policy::{ChangedPath, CommitInfo, DiffClass, LandContext, Policy, SignatureState};
use bee_proto::v1::{self, ErrorReason};
use bee_store::{Expected, LandedEntry, RefStep, RefUpdate, RepoId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::auth::{Caller, Usage};
use crate::convert;
use crate::errors::{ApiError, ApiResult};
use crate::kv::KvOp;
use crate::state::{AppState, RepoCtx, RequestMeta};

/// Namespace of coalescer owner leases.
pub const NS_LEASE: &str = "lease";

/// Request header of a forwarded land: the instance id of the owner the forwarder expects
/// at the endpoint. The forwarded marker is honoured only with this owner and a verifying
/// fence; a marker alone is ignored. A process with another id refuses the request before
/// authenticating it (a crashed owner's port may have been taken by any other server).
pub const OWNER_HEADER: &str = "x-bee-coalescer-owner";
/// Request header of a forwarded land: the fence of the lease the forwarder read. The owner
/// accepts the land only while it still holds exactly that fence.
pub const FENCE_HEADER: &str = "x-bee-coalescer-fence";
/// Subcode of the retryable `UNAVAILABLE` answer to a forwarded land that reached a process
/// which is not the owner (or not at the fence) the forwarder named.
pub const NOT_OWNER: &str = "not_coalescer_owner";

/// The refusal of a forwarded land by a process that is not the named owner.
pub fn not_owner(message: impl Into<String>) -> ApiError {
    let mut e = ApiError::new(ErrorReason::Unavailable, message);
    e.detail.subcode = NOT_OWNER.to_string();
    e
}

/// Checks a forwarded land's owner identity against this process and its lease: the
/// instance id must be ours and the lease of `name` must still be ours at `fence`, unexpired.
pub async fn verify_forwarded_owner(
    state: &AppState,
    repo: RepoId,
    name: &str,
    instance: &str,
    fence: u64,
) -> ApiResult<()> {
    if instance != state.instance_id || state.coalescers.stopped() {
        return Err(not_owner(format!(
            "forwarded to coalescer owner {instance}, reached {}",
            state.instance_id
        )));
    }
    match read_lease(state, repo, name).await? {
        Some(l)
            if l.owner == state.instance_id
                && l.fence == fence
                && l.expires_ms > state.now_ms() =>
        {
            Ok(())
        }
        _ => Err(not_owner(format!(
            "this process no longer owns the coalescer of {name} at fence {fence}"
        ))),
    }
}

/// One member of a land result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedMember {
    /// ChangeId.
    pub change_id: ChangeId,
    /// Changeset asked to land.
    pub source: ObjectId,
    /// Changeset on the ref.
    pub result: ObjectId,
    /// Already landed from the same source (idempotent retry).
    pub skipped: bool,
}

/// The result of a land.
#[derive(Clone, Debug)]
pub struct LandOutcome {
    /// Ref version after the land (the version of this stack's ref-log entry, or the
    /// recorded one when everything was skipped).
    pub version: u64,
    /// Ref target after this stack.
    pub target: ObjectId,
    /// Members, oldest first.
    pub members: Vec<LandedMember>,
    /// Attempts used.
    pub attempts: u32,
    /// Event sequence of the `ref.updated` event (0 when nothing was written).
    pub event_seq: u64,
}

/// A validated land request.
pub struct LandJob {
    /// The repo.
    pub repo: RepoCtx,
    /// The ref.
    pub ref_name: RefName,
    /// Stack head.
    pub head: ObjectId,
    /// Required current version (0 = any).
    pub expected_version: u64,
    /// Allow empty rebase results.
    pub allow_empty: bool,
    /// Change request on whose behalf (forge inputs).
    pub change_request_id: String,
    /// The caller.
    pub caller: Caller,
    /// Request facts.
    pub meta: RequestMeta,
}

struct Queued {
    job: LandJob,
    reply: oneshot::Sender<ApiResult<LandOutcome>>,
}

/// Owner tasks by `(repo, ref)`.
#[derive(Default)]
pub struct Coalescers {
    owners: Mutex<HashMap<(RepoId, String), mpsc::Sender<Queued>>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Set by [`Coalescers::shutdown`] and [`Coalescers::abort`]: no owner task starts again,
    /// so a land still in flight cannot take (or keep renewing) a lease for a stopped process.
    stopped: std::sync::atomic::AtomicBool,
}

impl Coalescers {
    /// True once the coalescers were shut down or aborted.
    pub fn stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Graceful stop: owners finish their queue, release their leases and exit.
    pub fn shutdown(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.owners
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// Crash: owner tasks stop where they are and keep their leases until expiry (used by
    /// failover tests to simulate a dead process).
    pub fn abort(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.owners
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        for t in self
            .tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            t.abort();
        }
    }

    /// Number of owner tasks this process runs.
    pub fn active(&self) -> usize {
        self.owners.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct LeaseRow {
    owner: String,
    fence: u64,
    expires_ms: u64,
    #[serde(default)]
    endpoint: String,
}

/// Current lease holder of a ref's coalescer, for tests and operators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseInfo {
    /// Owner process id.
    pub owner: String,
    /// Fencing token.
    pub fence: u64,
    /// Expiry.
    pub expires_ms: u64,
    /// Endpoint lands are forwarded to.
    pub endpoint: String,
}

fn lease_key(repo: RepoId, name: &str) -> Vec<u8> {
    format!("{}/{name}", repo.0.to_text()).into_bytes()
}

/// Reads the lease row of a ref's coalescer.
pub async fn read_lease(
    state: &AppState,
    repo: RepoId,
    name: &str,
) -> ApiResult<Option<LeaseInfo>> {
    Ok(state
        .kv
        .get(NS_LEASE, &lease_key(repo, name))
        .await?
        .and_then(|b| serde_json::from_slice::<LeaseRow>(&b).ok())
        .map(|r| LeaseInfo {
            owner: r.owner,
            fence: r.fence,
            expires_ms: r.expires_ms,
            endpoint: r.endpoint,
        }))
}

enum Acquire {
    Mine(u64),
    Held(LeaseRow),
}

/// Takes or renews the lease. `dead` is a lease row whose advertised endpoint was answered by
/// another process (see [`OWNER_HEADER`]): while the row is still exactly that one, it is
/// taken over as if expired. Safe by fencing: the fence moves on, so a live holder of the old
/// row could no longer publish.
async fn acquire(
    state: &AppState,
    repo: RepoId,
    name: &str,
    current_fence: Option<u64>,
    dead: Option<&LeaseRow>,
) -> ApiResult<Acquire> {
    let key = lease_key(repo, name);
    let ttl = state.cfg.land.owner_lease_ms;
    let endpoint = if state.cfg.server.advertise_url.is_empty() {
        state.public_url()
    } else {
        state.cfg.server.advertise_url.clone()
    };
    for _ in 0..20 {
        let now = state.now_ms();
        let cur = state.kv.get(NS_LEASE, &key).await?;
        let row: Option<LeaseRow> = cur.as_deref().and_then(|b| serde_json::from_slice(b).ok());
        let fence = match &row {
            Some(r) if r.owner == state.instance_id && Some(r.fence) == current_fence => r.fence,
            Some(r) if r.owner == state.instance_id && current_fence.is_none() => r.fence + 1,
            Some(r) if r.owner != state.instance_id && r.expires_ms > now && dead != Some(r) => {
                return Ok(Acquire::Held(r.clone()));
            }
            Some(r) if r.owner == state.instance_id => {
                // Our fence moved under us: someone took over and we got it back later.
                return Ok(Acquire::Held(r.clone()));
            }
            Some(r) => r.fence + 1,
            None => 1,
        };
        let new = LeaseRow {
            owner: state.instance_id.clone(),
            fence,
            expires_ms: now + ttl,
            endpoint: endpoint.clone(),
        };
        let ok = state
            .kv
            .apply(vec![
                KvOp::expect(NS_LEASE, key.clone(), cur),
                KvOp::put(
                    NS_LEASE,
                    key.clone(),
                    serde_json::to_vec(&new).unwrap_or_default(),
                ),
            ])
            .await?;
        if ok {
            if current_fence != Some(fence) {
                state
                    .metrics
                    .owner_changes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return Ok(Acquire::Mine(fence));
        }
    }
    Err(ApiError::new(
        ErrorReason::Unavailable,
        "coalescer lease contended",
    ))
}

async fn release(state: &AppState, repo: RepoId, name: &str, fence: u64) {
    let key = lease_key(repo, name);
    if let Ok(Some(cur)) = state.kv.get(NS_LEASE, &key).await
        && let Ok(row) = serde_json::from_slice::<LeaseRow>(&cur)
        && row.owner == state.instance_id
        && row.fence == fence
    {
        let expired = LeaseRow {
            expires_ms: 0,
            ..row
        };
        let _ = state
            .kv
            .apply(vec![
                KvOp::expect(NS_LEASE, key.clone(), Some(cur)),
                KvOp::put(
                    NS_LEASE,
                    key,
                    serde_json::to_vec(&expired).unwrap_or_default(),
                ),
            ])
            .await;
    }
}

/// Lands a stack: through the ref's coalescer for protected refs, by plain CAS otherwise.
pub async fn land(
    state: &Arc<AppState>,
    job: LandJob,
    protected: bool,
    forwarded: bool,
) -> ApiResult<LandOutcome> {
    if !protected || forwarded {
        return land_direct(state, job).await;
    }
    if state.coalescers.stopped() {
        return Err(ApiError::new(
            ErrorReason::Unavailable,
            "this server is stopping; retry the land",
        ));
    }
    let key = (job.repo.repo.id, job.ref_name.as_str().to_string());
    let (tx, rx) = oneshot::channel();
    let mut queued = Queued { job, reply: tx };
    for _ in 0..3 {
        let sender = {
            let mut owners = state
                .coalescers
                .owners
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            owners
                .entry(key.clone())
                .or_insert_with(|| spawn_owner(state.clone(), key.0, key.1.clone()))
                .clone()
        };
        match sender.send(queued).await {
            Ok(()) => {
                return rx.await.map_err(|_| {
                    ApiError::new(ErrorReason::Unavailable, "land coalescer stopped")
                })?;
            }
            Err(mpsc::error::SendError(back)) => {
                // The owner task exited between lookup and send: start a fresh one.
                queued = back;
                state
                    .coalescers
                    .owners
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&key);
            }
        }
    }
    land_direct(state, queued.job).await
}

fn spawn_owner(state: Arc<AppState>, repo: RepoId, name: String) -> mpsc::Sender<Queued> {
    let (tx, rx) = mpsc::channel::<Queued>(65_536);
    let handle = tokio::spawn(owner_loop(state.clone(), repo, name, rx));
    let mut tasks = state
        .coalescers
        .tasks
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    tasks.retain(|t| !t.is_finished());
    tasks.push(handle);
    tx
}

async fn owner_loop(
    state: Arc<AppState>,
    repo: RepoId,
    name: String,
    mut rx: mpsc::Receiver<Queued>,
) {
    let renew_every = Duration::from_millis(state.cfg.land.owner_lease_ms / 3);
    let idle_limit = Duration::from_secs(30);
    let mut fence: Option<u64> = None;
    let mut last_renew = std::time::Instant::now();
    let mut idle_since = std::time::Instant::now();
    // A lease row whose endpoint answered as another process: taken over at the next acquire.
    let dead: DeadOwner = Arc::default();
    loop {
        // Hold (or try to take) the lease.
        let suspect = dead.lock().unwrap_or_else(|p| p.into_inner()).take();
        if fence.is_none() || last_renew.elapsed() >= renew_every || suspect.is_some() {
            match acquire(&state, repo, &name, fence, suspect.as_ref()).await {
                Ok(Acquire::Mine(f)) => {
                    fence = Some(f);
                    last_renew = std::time::Instant::now();
                }
                Ok(Acquire::Held(row)) => {
                    fence = None;
                    // Someone else owns the ref: route whatever is queued to them.
                    match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                        Ok(Some(q)) => {
                            let mut batch = vec![q];
                            while batch.len() < state.cfg.land.batch_limit {
                                match rx.try_recv() {
                                    Ok(q) => batch.push(q),
                                    Err(_) => break,
                                }
                            }
                            route_batch(&state, batch, Some(&row), &dead).await;
                            continue;
                        }
                        Ok(None) => return,
                        Err(_) => continue,
                    }
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            }
        }
        let wait = renew_every
            .saturating_sub(last_renew.elapsed())
            .max(Duration::from_millis(1));
        let first = match tokio::time::timeout(wait, rx.recv()).await {
            Ok(Some(q)) => q,
            Ok(None) => break,
            Err(_) => {
                if idle_since.elapsed() >= idle_limit {
                    let retired = {
                        let mut owners = state
                            .coalescers
                            .owners
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        let empty = rx.is_empty();
                        if empty {
                            owners.remove(&(repo, name.clone()));
                        }
                        empty
                    };
                    if retired {
                        rx.close();
                        // Serve anything that slipped in, then stop.
                        while let Ok(q) = rx.try_recv() {
                            let out = land_direct(&state, q.job).await;
                            let _ = q.reply.send(out);
                        }
                        break;
                    }
                }
                continue;
            }
        };
        idle_since = std::time::Instant::now();
        let mut batch = vec![first];
        while batch.len() < state.cfg.land.batch_limit {
            match rx.try_recv() {
                Ok(q) => batch.push(q),
                Err(_) => break,
            }
        }
        let Some(f) = fence else { continue };
        let (jobs, replies): (Vec<LandJob>, Vec<_>) =
            batch.into_iter().map(|q| (q.job, q.reply)).unzip();
        let res = process_batch(&state, &jobs, Some(f)).await;
        match res {
            Ok(results) => {
                for (r, reply) in results.into_iter().zip(replies) {
                    let _ = reply.send(r);
                }
            }
            Err(Deposed) => {
                // Lost the lease: re-route every job of the batch.
                fence = None;
                let holder = read_lease(&state, repo, &name)
                    .await
                    .ok()
                    .flatten()
                    .map(|l| LeaseRow {
                        owner: l.owner,
                        fence: l.fence,
                        expires_ms: l.expires_ms,
                        endpoint: l.endpoint,
                    });
                let batch = jobs
                    .into_iter()
                    .zip(replies)
                    .map(|(job, reply)| Queued { job, reply })
                    .collect();
                route_batch(&state, batch, holder.as_ref(), &dead).await;
            }
        }
    }
    if let Some(f) = fence {
        release(&state, repo, &name, f).await;
    }
}

/// True when `endpoint` accepts a connection quickly.
async fn reachable(endpoint: &str) -> bool {
    let Ok(ep) = tonic::transport::Endpoint::from_shared(endpoint.to_string()) else {
        return false;
    };
    ep.connect_timeout(Duration::from_millis(300))
        .connect()
        .await
        .is_ok()
}

/// The lease row a forward found answered by another process, shared between an owner task
/// and the forwards it spawned.
type DeadOwner = Arc<Mutex<Option<LeaseRow>>>;

/// Routes lands this process does not own: each is forwarded to the owner `holder` names
/// when its endpoint answers as that owner; otherwise the whole batch lands here with the
/// plain CAS algorithm, as one batch, so fallbacks never fight each other over the ref. A
/// forward refused because another process answers at the endpoint records the row in
/// `dead`, so the owner task takes the lease over (re-election).
async fn route_batch(
    state: &Arc<AppState>,
    batch: Vec<Queued>,
    holder: Option<&LeaseRow>,
    dead: &DeadOwner,
) {
    if let Some(row) = holder
        && !row.endpoint.is_empty()
        && row.endpoint != state.public_url()
        && reachable(&row.endpoint).await
    {
        for q in batch {
            let s = state.clone();
            let (row, dead) = (row.clone(), dead.clone());
            tokio::spawn(async move {
                let out = forward_or_direct(&s, q.job, &row, &dead).await;
                let _ = q.reply.send(out);
            });
        }
        return;
    }
    let (jobs, replies): (Vec<LandJob>, Vec<_>) =
        batch.into_iter().map(|q| (q.job, q.reply)).unzip();
    let results = match process_batch(state, &jobs, None).await {
        Ok(r) => r,
        Err(Deposed) => (0..jobs.len())
            .map(|_| {
                Err(ApiError::new(
                    ErrorReason::Internal,
                    "direct land cannot be deposed",
                ))
            })
            .collect(),
    };
    for (r, reply) in results.into_iter().zip(replies) {
        let _ = reply.send(r);
    }
}

/// Forwards a land to the owning process, or lands directly when it cannot be reached or
/// another process answers in its place.
async fn forward_or_direct(
    state: &Arc<AppState>,
    job: LandJob,
    owner: &LeaseRow,
    dead: &DeadOwner,
) -> ApiResult<LandOutcome> {
    if !owner.endpoint.is_empty() && owner.endpoint != state.public_url() {
        match forward(owner, &job).await {
            Ok(out) => return Ok(out),
            Err(ForwardError::Remote(e)) => return Err(e),
            Err(ForwardError::NotOwner) => {
                *dead.lock().unwrap_or_else(|p| p.into_inner()) = Some(owner.clone());
            }
            Err(ForwardError::Unreachable) => {}
        }
    }
    land_direct(state, job).await
}

enum ForwardError {
    Unreachable,
    /// The endpoint answered as another process, or as the owner without that fence.
    NotOwner,
    Remote(ApiError),
}

async fn forward(owner: &LeaseRow, job: &LandJob) -> Result<LandOutcome, ForwardError> {
    use bee_proto::v1::write_service_client::WriteServiceClient;
    let endpoint = owner.endpoint.as_str();
    let channel = tonic::transport::Endpoint::from_shared(endpoint.to_string())
        .map_err(|_| ForwardError::Unreachable)?
        .connect_timeout(Duration::from_millis(500))
        .connect()
        .await
        .map_err(|_| ForwardError::Unreachable)?;
    let mut client = WriteServiceClient::new(channel);
    let mut req = tonic::Request::new(v1::LandRequest {
        repo: Some(v1::RepoRef {
            repo: Some(v1::repo_ref::Repo::Id(job.repo.id_text())),
        }),
        r#ref: job.ref_name.as_str().to_string(),
        head: Some(convert::id_to(&job.head)),
        expected_version: job.expected_version,
        allow_empty: job.allow_empty,
        change_request_id: job.change_request_id.clone(),
        ..Default::default()
    });
    req.set_timeout(job.meta.remaining().max(Duration::from_millis(100)));
    if let Ok(v) = format!("Bearer {}", job.caller.token_text).parse() {
        req.metadata_mut().insert("authorization", v);
    }
    req.metadata_mut().insert(
        "x-bee-forwarded",
        tonic::metadata::MetadataValue::from_static("1"),
    );
    if let Ok(v) = owner.owner.parse() {
        req.metadata_mut().insert(OWNER_HEADER, v);
    }
    req.metadata_mut().insert(
        FENCE_HEADER,
        tonic::metadata::MetadataValue::from(owner.fence),
    );
    match client.land(req).await {
        Ok(resp) => {
            let r = resp.into_inner();
            let target = r.r#ref.as_ref().and_then(|r| {
                convert::opt_id_from(r.target.as_ref(), "target")
                    .ok()
                    .flatten()
            });
            let members = r
                .landed
                .iter()
                .filter_map(|l| {
                    Some(LandedMember {
                        change_id: convert::change_id_from(l.change_id.as_ref(), "change_id")
                            .ok()?,
                        source: convert::id_from(l.source.as_ref(), "source").ok()?,
                        result: convert::id_from(l.result.as_ref(), "result").ok()?,
                        skipped: l.skipped,
                    })
                })
                .collect();
            Ok(LandOutcome {
                version: r.r#ref.as_ref().map_or(0, |r| r.version),
                target: target.unwrap_or(job.head),
                members,
                attempts: r.attempts,
                event_seq: r.event_seq,
            })
        }
        // Every answer a Bee server gives carries an ErrorDetail (SPEC §12.2). A status
        // without one was made below the API: the owner died mid-request (a reset stream, a
        // closed connection: UNKNOWN or INTERNAL "transport error"), or a proxy answered for it.
        // Whether the owner published the land first is unknown, so land directly: the landed
        // index answers `skipped` if it did. Seen with three replicas when the owner was
        // killed under load (docs/scaling.md). The caller's own deadline or cancellation is
        // still the caller's answer.
        Err(s)
            if bee_proto::errors::detail_from_status(&s).is_none()
                && !matches!(
                    s.code(),
                    tonic::Code::DeadlineExceeded | tonic::Code::Cancelled
                ) =>
        {
            Err(ForwardError::Unreachable)
        }
        Err(s)
            if bee_proto::errors::detail_from_status(&s)
                .is_some_and(|d| d.subcode == NOT_OWNER) =>
        {
            Err(ForwardError::NotOwner)
        }
        Err(s) => {
            let detail = bee_proto::errors::detail_from_status(&s).unwrap_or_else(|| {
                bee_proto::errors::error_detail(
                    bee_proto::errors::default_reason(s.code()),
                    s.message(),
                )
            });
            Err(ForwardError::Remote(ApiError {
                detail: Box::new(detail),
            }))
        }
    }
}

/// The plain land algorithm (SPEC §7.4 steps 1–5): a batch of one.
pub async fn land_direct(state: &Arc<AppState>, job: LandJob) -> ApiResult<LandOutcome> {
    let jobs = vec![job];
    match process_batch(state, &jobs, None).await {
        Ok(mut r) => r
            .pop()
            .unwrap_or_else(|| Err(ApiError::new(ErrorReason::Internal, "empty batch"))),
        Err(Deposed) => Err(ApiError::new(
            ErrorReason::Internal,
            "direct land cannot be deposed",
        )),
    }
}

/// The owner lost its lease before publishing.
struct Deposed;

enum Plan {
    Done(ApiResult<LandOutcome>),
    Accept {
        steps: Vec<StackStep>,
        skipped: Vec<LandedMember>,
        all: Vec<ObjectId>,
    },
}

fn needs_classes(policy: &Policy, ref_name: &str, is_agent: bool) -> bool {
    let doc = policy.doc();
    let bodies = doc
        .land
        .iter()
        .filter(|(p, _)| p.matches(ref_name))
        .map(|(_, r)| &r.body)
        .chain(
            doc.rulesets
                .values()
                .filter(|r| r.refs.iter().any(|p| p.matches(ref_name)))
                .map(|r| &r.body),
        );
    for b in bodies {
        if !b.approvals.is_empty() {
            return true;
        }
        if is_agent
            && !b.agents_may_land_without_human.is_empty()
            && b.agents_may_land_without_human.len() < DiffClass::ALL.len()
        {
            return true;
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
async fn plan_job(
    state: &Arc<AppState>,
    job: &LandJob,
    tip: &ObjectId,
    version: u64,
    running: &ObjectId,
    policy: &Policy,
    in_batch: &HashMap<ChangeId, (ObjectId, ObjectId)>,
    history: &bee_history::History,
) -> Plan {
    let fail = |e: ApiError| Plan::Done(Err(e));
    if job.expected_version != 0 && job.expected_version != version {
        let mut e = ApiError::new(
            ErrorReason::VersionMismatch,
            format!(
                "ref {} is at version {version}, not {}",
                job.ref_name.as_str(),
                job.expected_version
            ),
        );
        e.detail.payload = Some(v1::error_detail::Payload::VersionMismatch(
            v1::VersionMismatch {
                r#ref: job.ref_name.as_str().to_string(),
                expected_version: job.expected_version,
                current_version: version,
                current_target: Some(convert::id_to(tip)),
            },
        ));
        return fail(e);
    }

    let stack = match history.land_stack(&job.head, tip).await {
        Ok(s) => s,
        Err(e) => return fail(e.into()),
    };
    if stack.is_empty() {
        // The head is already contained in the ref: report what landed it, if anything.
        let cs = match history.changeset(&job.head).await {
            Ok(c) => c,
            Err(e) => return fail(e.into()),
        };
        let landed = state
            .meta
            .get_landed(job.repo.repo.id, &job.ref_name, &[*cs.change_id()])
            .await
            .ok()
            .and_then(|mut v| v.pop().flatten());
        return Plan::Done(Ok(LandOutcome {
            version: landed.as_ref().map_or(version, |l| l.version),
            target: *tip,
            members: landed
                .map(|l| {
                    vec![LandedMember {
                        change_id: l.change_id,
                        source: l.source,
                        result: l.result,
                        skipped: true,
                    }]
                })
                .unwrap_or_default(),
            attempts: 1,
            event_seq: 0,
        }));
    }
    let mut changesets = Vec::with_capacity(stack.len());
    for id in &stack {
        match history.changeset(id).await {
            Ok(c) => changesets.push(c),
            Err(e) => return fail(e.into()),
        }
    }
    let change_ids: Vec<ChangeId> = changesets.iter().map(|c| *c.change_id()).collect();
    let landed = match state
        .meta
        .get_landed(job.repo.repo.id, &job.ref_name, &change_ids)
        .await
    {
        Ok(l) => l,
        Err(e) => return fail(e.into()),
    };
    let mut skipped = Vec::new();
    let mut pending = Vec::new();
    for ((id, cid), row) in stack.iter().zip(&change_ids).zip(landed) {
        let existing = in_batch
            .get(cid)
            .copied()
            .or(row.map(|r| (r.source, r.result)));
        match existing {
            Some((source, result)) if source == *id => skipped.push(LandedMember {
                change_id: *cid,
                source,
                result,
                skipped: true,
            }),
            Some((source, _)) => {
                let mut e = ApiError::new(
                    ErrorReason::AlreadyLanded,
                    format!(
                        "change {} already landed on {} from {}",
                        cid.to_text(),
                        job.ref_name.as_str(),
                        source.to_text()
                    ),
                );
                e.detail.subcode = "already_landed".into();
                return fail(e);
            }
            None => pending.push(*id),
        }
    }
    if pending.is_empty() {
        let last = skipped.last().map_or(*tip, |m| m.result);
        let version = state
            .meta
            .get_landed(
                job.repo.repo.id,
                &job.ref_name,
                &[skipped.last().map(|m| m.change_id).unwrap_or(change_ids[0])],
            )
            .await
            .ok()
            .and_then(|mut v| v.pop().flatten())
            .map_or(version, |l| l.version);
        return Plan::Done(Ok(LandOutcome {
            version,
            target: last,
            members: skipped,
            attempts: 1,
            event_seq: 0,
        }));
    }
    // Policy: access to every changed path, the land rule, leases, push rules, rulesets.
    let mut paths: Vec<String> = Vec::new();
    let classes_needed = needs_classes(policy, job.ref_name.as_str(), job.caller.is_agent());
    let derived = state.derived(&job.repo);
    let mut changes = Vec::new();
    let mut commits = Vec::new();
    // Merges may be stack members (SPEC §7.4); a land rule with require_linear refuses them.
    let mut has_merges = false;
    for (id, cs) in stack.iter().zip(&changesets) {
        if !pending.contains(id) {
            continue;
        }
        has_merges |= cs.parents().len() > 1;
        let per_path: HashMap<String, DiffClass> = if classes_needed {
            match derived.diff_class(id).await {
                Ok(v) => v
                    .per_path
                    .iter()
                    .filter_map(|(p, c, _)| Some((p.to_string(), DiffClass::parse(c.as_str())?)))
                    .collect(),
                Err(e) => return fail(e.into()),
            }
        } else {
            HashMap::new()
        };
        for e in cs.delta() {
            let p = e.path.to_string();
            paths.push(p.clone());
            changes.push(ChangedPath {
                class: per_path.get(&p).copied().unwrap_or(DiffClass::Logic),
                size: match e.op {
                    DeltaOp::Put { size, .. } => Some(size),
                    DeltaOp::Delete => None,
                },
                path: p,
            });
        }
        commits.push(CommitInfo {
            id: id.to_text(),
            signature: SignatureState::Missing,
        });
    }
    paths.sort();
    paths.dedup();
    // Token scope over the stack as planned. The Land handler already ran the token's Datalog
    // half (and policy access) over the stack's paths in parallel, outside this serial section;
    // policy access at the publication tip is part of `evaluate_land` below.
    let scope_req = bee_auth::Request::new(job.repo.full_name.clone(), bee_policy::Action::Land)
        .paths(paths.iter().cloned())
        .on_ref(job.ref_name.as_str());
    let mut scope = job.caller.token.scope.clone();
    scope.repos = None;
    if let Err(e) = scope.check(&scope_req) {
        return fail(e.into());
    }
    // The route that submitted this job is not authority to bypass a merge queue.  Check the
    // stored queue entry in this serial planning section so a ruleset becoming active while a
    // change-request land is in flight is still enforced before publication.
    if let Err(e) = state
        .forge
        .require_landing_queue_entry(
            &job.repo,
            job.ref_name.as_str(),
            &job.head,
            &job.change_request_id,
        )
        .await
    {
        return fail(e);
    }
    let inputs = match state
        .forge
        .land_inputs(
            job.repo.repo.id,
            job.ref_name.as_str(),
            &job.head,
            &job.change_request_id,
        )
        .await
    {
        Ok(i) => i,
        Err(e) => return fail(e),
    };
    if policy.requires_signatures(job.ref_name.as_str()) {
        let ids: Vec<ObjectId> = commits
            .iter()
            .filter_map(|c| ObjectId::parse(&c.id).ok())
            .collect();
        match crate::signatures::states(state, &job.repo, &ids).await {
            Ok(states) => {
                for (c, s) in commits.iter_mut().zip(states) {
                    c.signature = s;
                }
            }
            Err(e) => return fail(e),
        }
    }
    let code_alerts = if policy.requires_code_scanning(job.ref_name.as_str()) {
        match state
            .forge
            .code_alerts(
                job.repo.repo.id,
                job.ref_name.as_str(),
                &job.head,
                &job.change_request_id,
            )
            .await
        {
            Ok(a) => a,
            Err(e) => return fail(e),
        }
    } else {
        Vec::new()
    };
    // The chain with its forge repo roles (`@repo-role/<role>`, SPEC §9.4), as the Land
    // handler's gate resolved them.
    let chain = match state.policy_actors(&job.caller, &job.repo).await {
        Ok(a) => a.to_vec(),
        Err(e) => return fail(e),
    };
    let decision = policy.evaluate_land(&LandContext {
        chain,
        ref_name: job.ref_name.as_str().to_string(),
        changes,
        head_revision: job.head.to_text(),
        last_pusher: inputs.last_pusher,
        reviews: inputs.reviews,
        checks: inputs.checks,
        leases: inputs.leases,
        commits,
        has_merges,
        required_workflows: inputs.required_workflows,
        code_alerts,
    });
    crate::services::policy::record_land(
        state.clone(),
        job.repo.clone(),
        job.ref_name.as_str().to_string(),
        job.caller.id().to_string(),
        job.head,
        decision.clone(),
    );
    if !decision.allowed {
        let reason = match decision.reason {
            Some(bee_policy::Reason::Leased) => ErrorReason::Leased,
            _ => ErrorReason::PolicyDenied,
        };
        let missing: Vec<String> = decision
            .missing_ids()
            .iter()
            .map(|s| s.to_string())
            .collect();
        return fail(ApiError::policy(
            reason,
            decision.rule.as_deref().unwrap_or("policy"),
            missing.clone(),
            format!("land refused by policy; missing: {}", missing.join(", ")),
        ));
    }
    let opts = RebaseOptions {
        allow_empty: job.allow_empty,
        ..RebaseOptions::default()
    };
    match history
        .rebase_stack(&pending, running, &opts, state.now_ms())
        .await
    {
        Ok(StackOutcome::Rebased(steps)) => Plan::Accept {
            steps,
            skipped,
            all: stack,
        },
        Ok(StackOutcome::Conflict {
            source, conflicts, ..
        }) => fail(convert::conflict_error(&conflicts, running, &source)),
        Err(e) => fail(e.into()),
    }
}

struct Accepted {
    index: usize,
    members: Vec<LandedMember>,
    target: ObjectId,
}

/// Processes queued lands for one ref; with `fence`, publication requires still owning the
/// ref's coalescer lease with that fencing token.
async fn process_batch(
    state: &Arc<AppState>,
    jobs: &[LandJob],
    fence: Option<u64>,
) -> Result<Vec<ApiResult<LandOutcome>>, Deposed> {
    let mut results: Vec<Option<ApiResult<LandOutcome>>> = (0..jobs.len()).map(|_| None).collect();
    let Some(first) = jobs.first() else {
        return Ok(Vec::new());
    };
    let repo = first.repo.clone();
    let name = first.ref_name.clone();
    let budget = state.cfg.land.attempt_budget;
    // Identical lands in one batch (a client retry racing its first attempt) are planned once
    // and answered with the first one's result, members marked skipped.
    let mut first_by_head: HashMap<(ObjectId, u64), usize> = HashMap::new();
    let dup_of: Vec<Option<usize>> = jobs
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let k = (j.head, j.expected_version);
            match first_by_head.get(&k) {
                Some(f) => Some(*f),
                None => {
                    first_by_head.insert(k, i);
                    None
                }
            }
        })
        .collect();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let current = match state.meta.get_ref(repo.repo.id, &name).await {
            Ok(r) => r,
            Err(e) => return Ok(fill(results, e.into())),
        };
        let Some((tip, version)) = current
            .as_ref()
            .and_then(|r| r.target.map(|t| (t, r.version)))
        else {
            let e = ApiError::not_found(format!(
                "ref {} does not exist; create it first",
                name.as_str()
            ));
            return Ok(fill(results, e));
        };
        let policy = match state.policy_at(&repo, name.as_str(), Some(&tip)).await {
            Ok(p) => p,
            Err(e) => return Ok(fill(results, e)),
        };
        // One history handle per attempt: its tree and changeset caches serve every stack,
        // and its writes are buffered, so the batch's rebased stacks (each read back by the
        // next) are written in one transaction before the ref update instead of two each.
        let history = state.history(&repo).buffered();
        let mut running = tip;
        let mut in_batch: HashMap<ChangeId, (ObjectId, ObjectId)> = HashMap::new();
        let mut accepted: Vec<Accepted> = Vec::new();
        let mut steps: Vec<RefStep> = Vec::new();
        let mut events = Vec::new();
        for (i, job) in jobs.iter().enumerate() {
            if results[i].is_some() || dup_of[i].is_some() {
                continue;
            }
            match plan_job(
                state, job, &tip, version, &running, &policy, &in_batch, &history,
            )
            .await
            {
                Plan::Done(r) => {
                    results[i] = Some(r);
                }
                Plan::Accept {
                    steps: st,
                    skipped,
                    all,
                } => {
                    let mut members = skipped;
                    for s in &st {
                        in_batch.insert(s.change_id, (s.source, s.result));
                        members.push(LandedMember {
                            change_id: s.change_id,
                            source: s.source,
                            result: s.result,
                            skipped: false,
                        });
                    }
                    // Keep stack order.
                    members.sort_by_key(|m| {
                        all.iter()
                            .position(|x| *x == m.source)
                            .unwrap_or(usize::MAX)
                    });
                    let target = st.last().map_or(running, |s| s.result);
                    let old = running;
                    running = target;
                    steps.push(RefStep {
                        target: Some(target),
                        landed: st
                            .iter()
                            .map(|s| LandedEntry {
                                change_id: s.change_id,
                                source: s.source,
                                result: s.result,
                            })
                            .collect(),
                    });
                    events.push(crate::events::new_event(
                        "ref.updated",
                        &json!({
                            "ref": name.as_str(),
                            "old": old.to_text(),
                            "new": target.to_text(),
                            "version": version + steps.len() as u64,
                            "principal_chain": job.caller.chain_ids(),
                            "landed": st.iter().map(|s| json!({
                                "change_id": s.change_id.to_text(),
                                "source": s.source.to_text(),
                                "result": s.result.to_text(),
                            })).collect::<Vec<_>>(),
                            "request_id": job.meta.request_id,
                        }),
                        state.now_ms(),
                    ));
                    accepted.push(Accepted {
                        index: i,
                        members,
                        target,
                    });
                }
            }
        }
        if accepted.is_empty() {
            return Ok(resolve_dups(results, &dup_of));
        }
        // Every row the stacks created, durable before the CAS that makes them reachable.
        if let Err(e) = history.flush_buffered().await {
            return Ok(publication_failed(results, e.into()));
        }
        if let Some(f) = fence {
            // Renew-and-check: succeeds only while the lease row still names this process
            // with the same fencing token (a long batch extends its own lease).
            match acquire(state, repo.repo.id, name.as_str(), Some(f), None).await {
                Ok(Acquire::Mine(g)) if g == f => {}
                // Deposed: publish nothing; the caller re-routes every job of the batch.
                Ok(_) | Err(_) => return Err(Deposed),
            }
        }
        let principals: Vec<String> = accepted
            .iter()
            .map(|a| jobs[a.index].caller.id().to_string())
            .collect();
        let principal = if principals.iter().all(|p| *p == principals[0]) {
            principals[0].clone()
        } else {
            format!("coalescer:{}", state.instance_id)
        };
        let k = steps.len();
        let update = RefUpdate {
            repo: repo.repo.id,
            name: name.clone(),
            expected: Expected::Version(version),
            steps,
            principal,
            at_ms: state.now_ms(),
            events,
        };
        match state.writer.update_ref(update).await {
            Ok(updated) => {
                state.metrics.batch(k);
                state
                    .metrics
                    .events
                    .fetch_add(k as u64, std::sync::atomic::Ordering::Relaxed);
                for (pos, a) in accepted.into_iter().enumerate() {
                    let landed_new = a.members.iter().filter(|m| !m.skipped).count() as u64;
                    let job = &jobs[a.index];
                    // Budgets: landed changes count against the token chain and its root.
                    let charged = state
                        .charge(
                            &job.caller,
                            Usage {
                                landed: landed_new,
                                ..Usage::default()
                            },
                        )
                        .await;
                    if let Err(e) = charged {
                        crate::logging::log(
                            crate::logging::Level::Warn,
                            "land",
                            "landed beyond budget (charged after publication)",
                            &[("error", e.detail.message.into())],
                        );
                    }
                    results[a.index] = Some(Ok(LandOutcome {
                        version: updated.old_version + pos as u64 + 1,
                        target: a.target,
                        members: a.members,
                        attempts: attempt,
                        event_seq: updated.event_seqs.get(pos).copied().unwrap_or(0),
                    }));
                }
                return Ok(resolve_dups(results, &dup_of));
            }
            Err(e)
                if matches!(
                    e.reason,
                    bee_core::Reason::VersionMismatch | bee_core::Reason::AlreadyLanded
                ) =>
            {
                state
                    .metrics
                    .land_cas_retries
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::logging::log(
                    crate::logging::Level::Debug,
                    "land",
                    "ref CAS lost; retrying the batch",
                    &[
                        ("reason", e.reason.as_str().into()),
                        ("message", e.message.clone().into()),
                        ("attempt", attempt.into()),
                    ],
                );
                if attempt >= budget {
                    let e = ApiError::new(
                        ErrorReason::LandContention,
                        format!("land attempt budget ({budget}) ran out under contention"),
                    );
                    return Ok(fill(results, e));
                }
                // Every job is planned again from a fresh read (SPEC §7.4 step 5).
                for r in results.iter_mut() {
                    *r = None;
                }
                // Full-jitter exponential backoff: 2 ms doubling to 200 ms.
                let cap_us = (2_000u64 << attempt.min(7)).min(200_000);
                let r = crate::util::random_bytes(4)
                    .map(|b| u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
                    .unwrap_or(0);
                tokio::time::sleep(Duration::from_micros(r % cap_us.max(1))).await;
            }
            Err(e) => {
                let e: ApiError = e.into();
                return Ok(publication_failed(results, e));
            }
        }
    }
}

fn resolve_dups(
    mut results: Vec<Option<ApiResult<LandOutcome>>>,
    dup_of: &[Option<usize>],
) -> Vec<ApiResult<LandOutcome>> {
    for (i, d) in dup_of.iter().enumerate() {
        if let Some(f) = d {
            results[i] = results[*f].clone().map(|r| {
                r.map(|mut o| {
                    for m in &mut o.members {
                        m.skipped = true;
                    }
                    o.event_seq = 0;
                    o
                })
            });
        }
    }
    results
        .into_iter()
        .map(|r| {
            r.unwrap_or_else(|| {
                Err(ApiError::new(
                    ErrorReason::Internal,
                    "land produced no result",
                ))
            })
        })
        .collect()
}

fn fill(
    results: Vec<Option<ApiResult<LandOutcome>>>,
    err: ApiError,
) -> Vec<ApiResult<LandOutcome>> {
    results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err(err.clone())))
        .collect()
}

/// A failed publication invalidates every outcome planned against this batch's running tip.
/// Unlike setup failures, this is reached only after at least one accepted stack exists.
fn publication_failed(
    results: Vec<Option<ApiResult<LandOutcome>>>,
    err: ApiError,
) -> Vec<ApiResult<LandOutcome>> {
    results.into_iter().map(|_| Err(err.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_failure_replaces_planned_batch_outcomes() {
        let results = vec![
            Some(Err(ApiError::new(
                ErrorReason::AlreadyLanded,
                "derived from an earlier stack in this batch",
            ))),
            None,
        ];
        let publication_error = ApiError::new(ErrorReason::Unavailable, "store unavailable");

        let results = publication_failed(results, publication_error);

        assert!(results.iter().all(|result| matches!(
            result,
            Err(error) if error.detail.reason == "UNAVAILABLE"
        )));
    }
}
