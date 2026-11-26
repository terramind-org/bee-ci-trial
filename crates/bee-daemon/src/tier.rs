//! One cache tier: the object read path shared by the node daemon and the regional cache.
//!
//! **Bytes and decisions are separate.** Object bytes are content-addressed and shared by every
//! caller and repo; whether a caller may *see* an object is a per-(credential, repo, object)
//! decision the tier only learns from the tier above:
//!
//! - an object the tier above served to this credential in this repo (positive decision), or
//! - an object `HasObjects` confirmed for this credential (one batched call per 256 ids).
//!
//! Decisions are trusted for `confirm_ms` after the last successful upstream call for that
//! credential and repo, and are all dropped when the repo's policy or redaction epoch changes
//! (seen in response trailers). When decisions are stale the tier asks the tier above; if that
//! cannot be reached it refuses (fail closed, SPEC §10.2) instead of serving from cache.
//!
//! **Coalescing.** Concurrent misses for one object share one upstream fetch, and concurrent
//! decision checks for one (credential, repo, object) share one `HasObjects`. Upstream
//! concurrency is capped by a semaphore; retryable failures are retried at most three times.
//!
//! **Refs.** `ResolveRef` answers are cached per credential for `ref_ttl_ms`, are never served
//! older than the request's `min_version` (read-your-writes, SPEC §6.3), and never go backwards.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bee_core::{ObjectId, ObjectKind};
use bee_proto::v1::{self, ErrorDetail, ErrorReason};
use bytes::Bytes;
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::coalesce::Coalescer;
use crate::error::{Result, err, invalid};
use crate::store::{ObjectStore, StoreStats, verify};
use crate::upstream::{Auth, AuthKey, Epochs, Upstream, with_retries};

/// Bytes per streamed chunk (the server's chunk size, so responses are byte-identical).
pub const CHUNK: usize = 1 << 20;
/// Ids per upstream `GetObjects` / `HasObjects`.
pub const BATCH: usize = 256;

/// Which tier this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TierKind {
    /// The node daemon (disk cache, unix socket).
    Daemon,
    /// A regional cache (memory, TCP).
    Regional,
}

impl TierKind {
    /// `ServerInfo.tier`: `daemon` or `cache`.
    pub fn info_name(self) -> &'static str {
        match self {
            TierKind::Daemon => "daemon",
            TierKind::Regional => "cache",
        }
    }

    /// `TierStats.tier`: `daemon` or `regional`.
    pub fn stats_name(self) -> &'static str {
        match self {
            TierKind::Daemon => "daemon",
            TierKind::Regional => "regional",
        }
    }
}

/// Tier settings.
#[derive(Clone, Debug)]
pub struct TierConfig {
    /// Daemon or regional.
    pub kind: TierKind,
    /// Ask the tier above for signed CDN URLs (SPEC §10.5), falling back to the API.
    pub signed_urls: bool,
    /// How long decisions stay trusted after the last upstream confirmation.
    pub confirm_ms: u64,
    /// How long a cached `ResolveRef` answer is served (at version 0).
    pub ref_ttl_ms: u64,
    /// Concurrent upstream requests.
    pub upstream_concurrency: usize,
    /// Retries of a retryable upstream failure.
    pub max_retries: u32,
}

impl TierConfig {
    /// Defaults for `kind`.
    pub fn new(kind: TierKind) -> Self {
        Self {
            kind,
            signed_urls: true,
            confirm_ms: 5_000,
            ref_ttl_ms: 1_000,
            upstream_concurrency: 32,
            max_retries: 3,
        }
    }
}

/// Counters since start (`GetTierStats`, `/metrics`).
#[derive(Debug, Default)]
pub struct Counters {
    /// Objects served from the cache without fetching bytes.
    pub hits: AtomicU64,
    /// Objects whose bytes had to come from the tier above.
    pub misses: AtomicU64,
    /// Reads refused because decisions could not be confirmed upstream.
    pub fail_closed: AtomicU64,
    /// Object bytes served to callers.
    pub bytes_served: AtomicU64,
    /// Requests sent to the tier above (all kinds).
    pub upstream_requests: AtomicU64,
    /// Object ids fetched from the tier above.
    pub upstream_objects: AtomicU64,
    /// `HasObjects` decision checks sent upstream.
    pub decision_checks: AtomicU64,
    /// Objects downloaded through signed URLs.
    pub signed_url_fetches: AtomicU64,
    /// Signed URLs that failed and fell back to the API.
    pub signed_url_fallbacks: AtomicU64,
    /// Upstream bytes that did not match their id.
    pub integrity_failures: AtomicU64,
    /// `ResolveRef` answers served from the cache.
    pub ref_hits: AtomicU64,
    /// `ResolveRef` answers fetched upstream.
    pub ref_misses: AtomicU64,
    /// Requests forwarded byte for byte.
    pub forwarded: AtomicU64,
}

fn inc(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

/// A JSON view of the counters and the store.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsView {
    /// Tier name.
    pub tier: &'static str,
    /// Cache hits.
    pub hits: u64,
    /// Cache misses.
    pub misses: u64,
    /// Hit rate over objects served (0 when nothing was served).
    pub hit_rate: f64,
    /// Fail-closed refusals.
    pub fail_closed: u64,
    /// Bytes served.
    pub bytes_served: u64,
    /// Upstream requests.
    pub upstream_requests: u64,
    /// Upstream object ids fetched.
    pub upstream_objects: u64,
    /// Decision checks.
    pub decision_checks: u64,
    /// Signed-URL downloads.
    pub signed_url_fetches: u64,
    /// Signed-URL fallbacks.
    pub signed_url_fallbacks: u64,
    /// Integrity failures upstream.
    pub integrity_failures: u64,
    /// Ref cache hits.
    pub ref_hits: u64,
    /// Ref cache misses.
    pub ref_misses: u64,
    /// Forwarded requests.
    pub forwarded: u64,
    /// The object store.
    pub store: StoreStats,
}

/// What a caller gets for one object.
#[derive(Clone, Debug)]
pub enum Item {
    /// The verified bytes and the object's access class in the repo.
    Served {
        /// Bytes.
        bytes: Bytes,
        /// `AccessClass` value.
        class: i32,
    },
    /// Cached and readable by the caller (answered without reading the bytes).
    Present {
        /// `AccessClass` value.
        class: i32,
    },
    /// A per-item failure as the tier above reported it.
    Status {
        /// `ItemStatus` value.
        status: i32,
        /// Reason name, e.g. `REDACTED`.
        reason: String,
    },
}

#[derive(Clone, Debug)]
enum Fetched {
    Ok {
        bytes: Bytes,
        class: i32,
        by: AuthKey,
    },
    Failed {
        status: i32,
        reason: String,
        by: AuthKey,
    },
    /// The bytes landed in the store before this caller could lead a fetch.
    Cached,
}

#[derive(Debug)]
struct RepoAuth {
    confirmed: Instant,
    allowed: HashSet<ObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RefKey {
    auth: AuthKey,
    repo: String,
    name: String,
    at_version: u64,
}

#[derive(Clone, Debug)]
struct CachedRef {
    at: Instant,
    resp: v1::ResolveRefResponse,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TreeKey {
    auth: AuthKey,
    repo: String,
    at: ObjectId,
    prefix: Vec<u8>,
}

const TREE_CACHE_ENTRIES: usize = 64;

/// A cached recursive listing.
type Listing = Arc<Vec<v1::TreeEntry>>;

/// The canonical cache key of a repo reference.
pub fn repo_key(repo: Option<&v1::RepoRef>) -> Result<String> {
    match repo.and_then(|r| r.repo.as_ref()) {
        Some(v1::repo_ref::Repo::FullName(n)) if !n.is_empty() => Ok(format!("name:{n}")),
        Some(v1::repo_ref::Repo::Id(i)) if !i.is_empty() => Ok(format!("id:{i}")),
        _ => Err(invalid("missing_field", "repo is required")),
    }
}

/// A proto id.
pub fn pid(id: &ObjectId) -> v1::ObjectId {
    v1::ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

fn ok_status(chunk: &v1::ObjectChunk) -> bool {
    chunk.status == v1::ItemStatus::Ok as i32
        || (chunk.status == v1::ItemStatus::Unspecified as i32 && chunk.error_reason.is_empty())
}

/// A cache tier over a store and the tier above.
pub struct Tier {
    cfg: TierConfig,
    store: Arc<dyn ObjectStore>,
    upstream: Arc<dyn Upstream>,
    fetches: Coalescer<ObjectId, Fetched>,
    checks: Coalescer<(AuthKey, String, ObjectId), bool>,
    ref_flights: Coalescer<RefKey, std::result::Result<v1::ResolveRefResponse, Box<ErrorDetail>>>,
    auth: Mutex<HashMap<(AuthKey, String), RepoAuth>>,
    classes: Mutex<HashMap<(String, ObjectId), i32>>,
    epochs: Mutex<HashMap<String, Epochs>>,
    refs: Mutex<HashMap<RefKey, CachedRef>>,
    trees: Mutex<Vec<(TreeKey, Listing)>>,
    info: Mutex<Option<(Instant, v1::ServerInfo)>>,
    limiter: Semaphore,
    /// Counters.
    pub counters: Counters,
    started_ms: i64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Tier {
    /// A tier serving from `store`, filling it from `upstream`.
    pub fn new(cfg: TierConfig, store: Arc<dyn ObjectStore>, upstream: Arc<dyn Upstream>) -> Self {
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        Self {
            limiter: Semaphore::new(cfg.upstream_concurrency.max(1)),
            cfg,
            store,
            upstream,
            fetches: Coalescer::new(),
            checks: Coalescer::new(),
            ref_flights: Coalescer::new(),
            auth: Mutex::new(HashMap::new()),
            classes: Mutex::new(HashMap::new()),
            epochs: Mutex::new(HashMap::new()),
            refs: Mutex::new(HashMap::new()),
            trees: Mutex::new(Vec::new()),
            info: Mutex::new(None),
            counters: Counters::default(),
            started_ms,
        }
    }

    /// The configuration.
    pub fn config(&self) -> &TierConfig {
        &self.cfg
    }

    /// The object store.
    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    /// The tier above.
    pub fn upstream(&self) -> &Arc<dyn Upstream> {
        &self.upstream
    }

    // ----- decisions and epochs ----------------------------------------------------------

    fn confirm_window(&self) -> Duration {
        Duration::from_millis(self.cfg.confirm_ms)
    }

    /// True while decisions for (credential, repo) are trusted.
    fn confirmed(&self, akey: AuthKey, repo: &str) -> bool {
        let mut map = lock(&self.auth);
        let key = (akey, repo.to_owned());
        match map.get(&key) {
            Some(ra) if ra.confirmed.elapsed() < self.confirm_window() => true,
            Some(_) => {
                map.remove(&key);
                false
            }
            None => false,
        }
    }

    fn allowed(&self, akey: AuthKey, repo: &str, id: &ObjectId) -> bool {
        self.confirmed(akey, repo)
            && lock(&self.auth)
                .get(&(akey, repo.to_owned()))
                .is_some_and(|ra| ra.allowed.contains(id))
    }

    fn confirm(&self, akey: AuthKey, repo: &str, allow: &[ObjectId]) {
        let mut map = lock(&self.auth);
        let ra = map
            .entry((akey, repo.to_owned()))
            .or_insert_with(|| RepoAuth {
                confirmed: Instant::now(),
                allowed: HashSet::new(),
            });
        ra.confirmed = Instant::now();
        ra.allowed.extend(allow.iter().copied());
    }

    /// The access class the tier above last reported for `id` in `repo`; when unknown (the
    /// bytes came through another repo, or before a restart) the conservative `RESTRICTED`,
    /// which makes tiers below decide per object.
    fn served_class(&self, repo: &str, id: &ObjectId) -> i32 {
        lock(&self.classes)
            .get(&(repo.to_owned(), *id))
            .copied()
            .unwrap_or(v1::AccessClass::Restricted as i32)
    }

    /// Records epochs the tier above reported for `repo`; a change drops every decision,
    /// access class, cached ref and listing of that repo.
    fn observe(&self, repo: &str, epochs: Option<&Epochs>) {
        let Some(e) = epochs else {
            return;
        };
        let changed = {
            let mut map = lock(&self.epochs);
            let changed = map.get(repo).is_some_and(|prev| prev != e);
            map.insert(repo.to_owned(), e.clone());
            changed
        };
        if changed {
            self.flush_repo(repo);
        }
    }

    /// Drops everything the tier learned about `repo` except object bytes.
    pub fn flush_repo(&self, repo: &str) {
        lock(&self.auth).retain(|(_, r), _| r != repo);
        lock(&self.classes).retain(|(r, _), _| r != repo);
        lock(&self.refs).retain(|k, _| k.repo != repo);
        lock(&self.trees).retain(|(k, _)| k.repo != repo);
    }

    /// The last epochs seen for `repo`.
    pub fn epochs_of(&self, repo: &str) -> Option<Epochs> {
        lock(&self.epochs).get(repo).cloned()
    }

    // ----- objects -----------------------------------------------------------------------

    /// `GetObjects` as the server answers it, served from the cache where decisions allow.
    pub async fn get_objects(
        &self,
        auth: &Auth,
        req: v1::GetObjectsRequest,
    ) -> Result<(Vec<v1::ObjectChunk>, Option<Epochs>)> {
        let repo = repo_key(req.repo.as_ref())?;
        let mut ids = Vec::with_capacity(req.ids.len());
        for raw in &req.ids {
            match ObjectId::from_bytes(&raw.raw) {
                Ok(id) if req.ids.len() <= BATCH => ids.push(id),
                // Malformed or oversized requests get the tier above's exact answer.
                _ => return self.passthrough_objects(auth, &repo, req).await,
            }
        }
        let repo_ref = req.repo.clone().unwrap_or_default();
        let items = self.items(auth, &repo, &repo_ref, &ids, true).await?;
        let mut out = Vec::new();
        for id in &ids {
            match items.get(id) {
                Some(Item::Served { bytes, class }) => {
                    inc(&self.counters.bytes_served, bytes.len() as u64);
                    push_chunks(&mut out, id, bytes, *class, req.resume_offset, req.length);
                }
                Some(Item::Status { status, reason }) => out.push(v1::ObjectChunk {
                    id: Some(pid(id)),
                    last: true,
                    status: *status,
                    error_reason: reason.clone(),
                    ..Default::default()
                }),
                // `Present` only answers byte-less lookups; here it cannot occur.
                Some(Item::Present { .. }) | None => out.push(v1::ObjectChunk {
                    id: Some(pid(id)),
                    last: true,
                    status: v1::ItemStatus::NotFound as i32,
                    error_reason: "NOT_FOUND".into(),
                    ..Default::default()
                }),
            }
        }
        Ok((out, self.epochs_of(&repo)))
    }

    async fn passthrough_objects(
        &self,
        auth: &Auth,
        repo: &str,
        req: v1::GetObjectsRequest,
    ) -> Result<(Vec<v1::ObjectChunk>, Option<Epochs>)> {
        inc(&self.counters.forwarded, 1);
        inc(&self.counters.upstream_requests, 1);
        let reply = self.upstream.get_objects(auth, req).await?;
        self.observe(repo, reply.epochs.as_ref());
        Ok((reply.msg, reply.epochs))
    }

    /// A cached object as an item: its verified bytes when `want_bytes`, else only the fact
    /// that it is held. `None` when the store does not have it.
    fn cached_item(&self, repo: &str, id: &ObjectId, want_bytes: bool) -> Option<Item> {
        let class = self.served_class(repo, id);
        let item = if want_bytes {
            Item::Served {
                bytes: self.store.get(id)?,
                class,
            }
        } else if self.store.contains(id) {
            Item::Present { class }
        } else {
            return None;
        };
        inc(&self.counters.hits, 1);
        Some(item)
    }

    /// The bytes (or per-item failure) of each id for this credential in this repo. With
    /// `want_bytes` false, cached objects are answered `Present` without being read.
    pub async fn items(
        &self,
        auth: &Auth,
        repo: &str,
        repo_ref: &v1::RepoRef,
        ids: &[ObjectId],
        want_bytes: bool,
    ) -> Result<HashMap<ObjectId, Item>> {
        let akey = auth.key();
        let mut items: HashMap<ObjectId, Item> = HashMap::new();
        let mut unique: Vec<ObjectId> = ids.to_vec();
        unique.sort();
        unique.dedup();
        let mut need_check = Vec::new();
        let mut need_fetch = Vec::new();
        for id in unique {
            if self.allowed(akey, repo, &id)
                && let Some(item) = self.cached_item(repo, &id, want_bytes)
            {
                items.insert(id, item);
            } else if self.store.contains(&id) {
                need_check.push(id);
            } else {
                need_fetch.push(id);
            }
        }
        if !need_check.is_empty() {
            let present = self.check(auth, repo, repo_ref, &need_check).await?;
            for id in need_check {
                match present
                    .get(&id)
                    .copied()
                    .unwrap_or(false)
                    .then(|| self.cached_item(repo, &id, want_bytes))
                    .flatten()
                {
                    Some(item) => {
                        items.insert(id, item);
                    }
                    None => need_fetch.push(id),
                }
            }
        }
        if need_fetch.is_empty() {
            return Ok(items);
        }
        let failure: Mutex<Option<Box<ErrorDetail>>> = Mutex::new(None);
        let failure_ref = &failure;
        let led = std::sync::atomic::AtomicUsize::new(0);
        let led_ref = &led;
        let got = self
            .fetches
            .run(
                &need_fetch,
                // Bytes that landed after this caller looked are not fetched again.
                |id| self.store.contains(id).then_some(Fetched::Cached),
                |lead| async move {
                    led_ref.fetch_add(lead.len(), Ordering::Relaxed);
                    match self.fetch(auth, repo, repo_ref, &lead).await {
                        Ok(m) => m,
                        Err(e) => {
                            *lock(failure_ref) = Some(e);
                            HashMap::new()
                        }
                    }
                },
            )
            .await;
        if let Some(e) = lock(&failure).take() {
            return Err(e);
        }
        let mut own = Vec::new();
        let mut shared = 0usize;
        for id in need_fetch {
            match got.get(&id) {
                Some(Fetched::Ok { bytes, class, by }) if *by == akey => {
                    shared += 1;
                    items.insert(
                        id,
                        Item::Served {
                            bytes: bytes.clone(),
                            class: *class,
                        },
                    );
                }
                Some(Fetched::Failed { status, reason, by }) if *by == akey => {
                    items.insert(
                        id,
                        Item::Status {
                            status: *status,
                            reason: reason.clone(),
                        },
                    );
                }
                // Already cached, fetched under another credential, or not at all: serve it if
                // this caller's decision is known, else it needs its own answer from above.
                _ => {
                    if self.allowed(akey, repo, &id)
                        && let Some(item) = self.cached_item(repo, &id, want_bytes)
                    {
                        items.insert(id, item);
                    } else {
                        own.push(id);
                    }
                }
            }
        }
        // Objects another caller's fetch delivered to this one are hits: no upstream bytes
        // moved for them on this request's behalf.
        inc(
            &self.counters.hits,
            shared.saturating_sub(led.load(Ordering::Relaxed)) as u64,
        );
        if !own.is_empty() {
            let mut still = Vec::new();
            let present = self.check(auth, repo, repo_ref, &own).await?;
            for id in own {
                match present
                    .get(&id)
                    .copied()
                    .unwrap_or(false)
                    .then(|| self.cached_item(repo, &id, want_bytes))
                    .flatten()
                {
                    Some(item) => {
                        items.insert(id, item);
                    }
                    None => still.push(id),
                }
            }
            if !still.is_empty() {
                for (id, f) in self.fetch(auth, repo, repo_ref, &still).await? {
                    let item = match f {
                        Fetched::Ok { bytes, class, .. } => Item::Served { bytes, class },
                        Fetched::Failed { status, reason, .. } => Item::Status { status, reason },
                        Fetched::Cached => continue,
                    };
                    items.insert(id, item);
                }
            }
        }
        Ok(items)
    }

    /// Decision checks through `HasObjects`, coalesced per (credential, repo, id).
    async fn check(
        &self,
        auth: &Auth,
        repo: &str,
        repo_ref: &v1::RepoRef,
        ids: &[ObjectId],
    ) -> Result<HashMap<ObjectId, bool>> {
        let akey = auth.key();
        let keys: Vec<(AuthKey, String, ObjectId)> =
            ids.iter().map(|id| (akey, repo.to_owned(), *id)).collect();
        let failure: Mutex<Option<Box<ErrorDetail>>> = Mutex::new(None);
        let failure_ref = &failure;
        let got = self
            .checks
            .run(
                &keys,
                |_| None,
                |lead| async move {
                    let lead_ids: Vec<ObjectId> = lead.iter().map(|k| k.2).collect();
                    match self.has_upstream(auth, repo, repo_ref, &lead_ids).await {
                        Ok(present) => lead.into_iter().zip(present).collect(),
                        Err(e) => {
                            *lock(failure_ref) = Some(e);
                            HashMap::new()
                        }
                    }
                },
            )
            .await;
        if let Some(e) = lock(&failure).take() {
            if crate::upstream::is_unreachable(&e) {
                inc(&self.counters.fail_closed, ids.len() as u64);
            }
            return Err(e);
        }
        Ok(got.into_iter().map(|(k, v)| (k.2, v)).collect())
    }

    async fn has_upstream(
        &self,
        auth: &Auth,
        repo: &str,
        repo_ref: &v1::RepoRef,
        ids: &[ObjectId],
    ) -> Result<Vec<bool>> {
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(BATCH) {
            let _permit = self.limiter.acquire().await;
            inc(&self.counters.upstream_requests, 1);
            inc(&self.counters.decision_checks, 1);
            let req = v1::HasObjectsRequest {
                repo: Some(repo_ref.clone()),
                ids: batch.iter().map(pid).collect(),
            };
            let reply = with_retries(self.cfg.max_retries, || {
                self.upstream.has_objects(auth, req.clone())
            })
            .await?;
            if reply.msg.present.len() != batch.len() {
                return Err(err(
                    ErrorReason::Internal,
                    "HasObjects answered a different number of ids",
                ));
            }
            self.observe(repo, reply.epochs.as_ref());
            let allow: Vec<ObjectId> = batch
                .iter()
                .zip(&reply.msg.present)
                .filter(|(_, p)| **p)
                .map(|(id, _)| *id)
                .collect();
            self.confirm(auth.key(), repo, &allow);
            out.extend(reply.msg.present);
        }
        Ok(out)
    }

    /// Fetches `ids` from the tier above for this credential, verifying and caching the bytes.
    async fn fetch(
        &self,
        auth: &Auth,
        repo: &str,
        repo_ref: &v1::RepoRef,
        ids: &[ObjectId],
    ) -> Result<HashMap<ObjectId, Fetched>> {
        let mut out = HashMap::new();
        for batch in ids.chunks(BATCH) {
            let mut retry = self
                .fetch_batch(auth, repo, repo_ref, batch, self.cfg.signed_urls, &mut out)
                .await?;
            if !retry.is_empty() {
                // Signed URLs that failed, or bytes that did not verify: once more through the
                // API without signed URLs.
                retry = self
                    .fetch_batch(auth, repo, repo_ref, &retry, false, &mut out)
                    .await?;
            }
            for id in retry {
                inc(&self.counters.integrity_failures, 1);
                out.insert(
                    id,
                    Fetched::Failed {
                        status: v1::ItemStatus::Unspecified as i32,
                        reason: "INTEGRITY".into(),
                        by: auth.key(),
                    },
                );
            }
            for id in batch {
                out.entry(*id).or_insert_with(|| Fetched::Failed {
                    status: v1::ItemStatus::NotFound as i32,
                    reason: "NOT_FOUND".into(),
                    by: auth.key(),
                });
            }
        }
        Ok(out)
    }

    async fn fetch_batch(
        &self,
        auth: &Auth,
        repo: &str,
        repo_ref: &v1::RepoRef,
        batch: &[ObjectId],
        signed: bool,
        out: &mut HashMap<ObjectId, Fetched>,
    ) -> Result<Vec<ObjectId>> {
        let akey = auth.key();
        let reply = {
            let _permit = self.limiter.acquire().await;
            inc(&self.counters.upstream_requests, 1);
            inc(&self.counters.upstream_objects, batch.len() as u64);
            inc(&self.counters.misses, batch.len() as u64);
            let req = v1::GetObjectsRequest {
                repo: Some(repo_ref.clone()),
                ids: batch.iter().map(pid).collect(),
                allow_signed_urls: signed,
                ..Default::default()
            };
            with_retries(self.cfg.max_retries, || {
                self.upstream.get_objects(auth, req.clone())
            })
            .await?
        };
        self.observe(repo, reply.epochs.as_ref());
        let mut retry = Vec::new();
        let mut parts: HashMap<ObjectId, Vec<u8>> = HashMap::new();
        let mut urls: Vec<(ObjectId, String, i32)> = Vec::new();
        let mut done: Vec<(ObjectId, Bytes, i32)> = Vec::new();
        for chunk in reply.msg {
            let Some(id) = chunk
                .id
                .as_ref()
                .and_then(|i| ObjectId::from_bytes(&i.raw).ok())
            else {
                continue;
            };
            if !batch.contains(&id) {
                continue;
            }
            if !ok_status(&chunk) {
                if chunk.status == v1::ItemStatus::Redacted as i32 {
                    // Fail closed on redaction: the bytes leave this tier at once.
                    self.store.remove(&id);
                }
                out.insert(
                    id,
                    Fetched::Failed {
                        status: chunk.status,
                        reason: chunk.error_reason,
                        by: akey,
                    },
                );
                continue;
            }
            if !chunk.signed_url.is_empty() {
                urls.push((id, chunk.signed_url, chunk.access_class));
                continue;
            }
            if chunk.encoding != v1::ObjectEncoding::Unspecified as i32
                && chunk.encoding != v1::ObjectEncoding::Raw as i32
            {
                retry.push(id);
                continue;
            }
            let buf = parts.entry(id).or_default();
            if chunk.offset as usize != buf.len() {
                parts.remove(&id);
                retry.push(id);
                continue;
            }
            buf.extend_from_slice(&chunk.data);
            if chunk.last {
                let bytes = parts.remove(&id).unwrap_or_default();
                done.push((id, Bytes::from(bytes), chunk.access_class));
            }
        }
        for (id, url, class) in urls {
            match self.upstream.fetch_url(&url).await {
                Ok(bytes) if verify(&id, &bytes) => {
                    inc(&self.counters.signed_url_fetches, 1);
                    done.push((id, bytes, class));
                }
                _ => {
                    inc(&self.counters.signed_url_fallbacks, 1);
                    retry.push(id);
                }
            }
        }
        let mut verified = Vec::with_capacity(done.len());
        for (id, bytes, class) in done {
            if verify(&id, &bytes) {
                verified.push((id, bytes, class));
            } else {
                retry.push(id);
            }
        }
        // Decisions first, then bytes: a caller that sees the bytes also sees the decision.
        {
            let mut classes = lock(&self.classes);
            for (id, _, class) in &verified {
                classes.insert((repo.to_owned(), *id), *class);
            }
        }
        let allow: Vec<ObjectId> = verified.iter().map(|(id, _, _)| *id).collect();
        self.confirm(akey, repo, &allow);
        for (id, bytes, class) in verified {
            // A store failure (disk full) still serves the verified bytes.
            let _ = self.store.put(&id, &bytes);
            out.insert(
                id,
                Fetched::Ok {
                    bytes,
                    class,
                    by: akey,
                },
            );
        }
        retry.retain(|id| !out.contains_key(id));
        retry.dedup();
        Ok(retry)
    }

    /// Makes sure `ids` are cached and readable by this credential; returns the per-item
    /// failures (empty when everything is cached).
    pub async fn ensure(
        &self,
        auth: &Auth,
        repo_ref: &v1::RepoRef,
        ids: &[ObjectId],
    ) -> Result<Vec<(ObjectId, String)>> {
        use futures_util::StreamExt as _;
        let repo = repo_key(Some(repo_ref))?;
        let repo = repo.as_str();
        let mut failures = Vec::new();
        // Batches run concurrently (the upstream semaphore still caps requests in flight).
        let batches: Vec<Vec<ObjectId>> = ids.chunks(BATCH).map(<[ObjectId]>::to_vec).collect();
        let mut results = futures_util::stream::iter(batches)
            .map(|batch: Vec<ObjectId>| async move {
                let items = self.items(auth, repo, repo_ref, &batch, false).await;
                (batch, items)
            })
            .buffer_unordered(8);
        while let Some((batch, items)) = results.next().await {
            let items = items?;
            for id in &batch {
                match items.get(id) {
                    Some(Item::Served { .. } | Item::Present { .. }) => {}
                    Some(Item::Status { reason, .. }) => failures.push((*id, reason.clone())),
                    None => failures.push((*id, "NOT_FOUND".into())),
                }
            }
        }
        failures.sort();
        Ok(failures)
    }

    /// `HasObjects`: always forwarded (a cached decision must not answer existence probes).
    pub async fn has_objects(
        &self,
        auth: &Auth,
        req: v1::HasObjectsRequest,
    ) -> Result<(v1::HasObjectsResponse, Option<Epochs>)> {
        let repo = repo_key(req.repo.as_ref())?;
        inc(&self.counters.upstream_requests, 1);
        let reply = self.upstream.has_objects(auth, req.clone()).await?;
        self.observe(&repo, reply.epochs.as_ref());
        let ids: Vec<ObjectId> = req
            .ids
            .iter()
            .filter_map(|i| ObjectId::from_bytes(&i.raw).ok())
            .collect();
        if ids.len() == reply.msg.present.len() {
            let allow: Vec<ObjectId> = ids
                .iter()
                .zip(&reply.msg.present)
                .filter(|(_, p)| **p)
                .map(|(id, _)| *id)
                .collect();
            self.confirm(auth.key(), &repo, &allow);
        }
        Ok((reply.msg, reply.epochs))
    }

    // ----- refs --------------------------------------------------------------------------

    async fn resolve_upstream(
        &self,
        auth: &Auth,
        repo: &str,
        req: v1::ResolveRefRequest,
    ) -> Result<v1::ResolveRefResponse> {
        inc(&self.counters.upstream_requests, 1);
        let reply = with_retries(self.cfg.max_retries, || {
            self.upstream.resolve_ref(auth, req.clone())
        })
        .await?;
        self.observe(repo, reply.epochs.as_ref());
        self.confirm(auth.key(), repo, &[]);
        Ok(reply.msg)
    }

    /// `ResolveRef` with a short-TTL cache, read-your-writes and no going backwards.
    pub async fn resolve_ref(
        &self,
        auth: &Auth,
        req: v1::ResolveRefRequest,
    ) -> Result<(v1::ResolveRefResponse, Option<Epochs>)> {
        let repo = repo_key(req.repo.as_ref())?;
        let akey = auth.key();
        let key = RefKey {
            auth: akey,
            repo: repo.clone(),
            name: req.name.clone(),
            at_version: req.at_version,
        };
        let version_of = |r: &v1::ResolveRefResponse| r.r#ref.as_ref().map_or(0, |r| r.version);
        let cached = lock(&self.refs).get(&key).cloned();
        if let Some(c) = &cached
            && self.confirmed(akey, &repo)
        {
            let fresh =
                req.at_version > 0 || c.at.elapsed() < Duration::from_millis(self.cfg.ref_ttl_ms);
            if fresh && version_of(&c.resp) >= req.min_version {
                inc(&self.counters.ref_hits, 1);
                return Ok((c.resp.clone(), self.epochs_of(&repo)));
            }
        }
        inc(&self.counters.ref_misses, 1);
        let repo_s = repo.as_str();
        let got = self
            .ref_flights
            .run(
                std::slice::from_ref(&key),
                |_| None,
                |keys| {
                    let r = req.clone();
                    async move {
                        let v = self.resolve_upstream(auth, repo_s, r).await;
                        keys.into_iter().map(|k| (k, v.clone())).collect()
                    }
                },
            )
            .await;
        let mut resp = match got.get(&key) {
            Some(Ok(r)) if version_of(r) >= req.min_version => r.clone(),
            // A shared answer older than this caller's min_version: ask again for it.
            Some(Ok(_)) | None => self.resolve_upstream(auth, repo_s, req.clone()).await?,
            Some(Err(e)) => return Err(e.clone()),
        };
        let mut refs = lock(&self.refs);
        if let Some(prev) = refs.get(&key)
            && req.at_version == 0
            && version_of(&prev.resp) > version_of(&resp)
            && version_of(&prev.resp) >= req.min_version
        {
            // Never go backwards: a lagging answer loses to the newer one already served.
            resp = prev.resp.clone();
        }
        refs.insert(
            key,
            CachedRef {
                at: Instant::now(),
                resp: resp.clone(),
            },
        );
        drop(refs);
        Ok((resp, self.epochs_of(&repo)))
    }

    // ----- trees -------------------------------------------------------------------------

    /// Every entry under `prefix` of changeset `at` (recursive), as this credential may list
    /// it. Listings of a changeset are immutable, so they are cached per credential while its
    /// decisions are confirmed.
    pub async fn list_tree(
        &self,
        auth: &Auth,
        repo_ref: &v1::RepoRef,
        at: &ObjectId,
        prefix: &[u8],
    ) -> Result<Arc<Vec<v1::TreeEntry>>> {
        let repo = repo_key(Some(repo_ref))?;
        let akey = auth.key();
        let key = TreeKey {
            auth: akey,
            repo: repo.clone(),
            at: *at,
            prefix: prefix.to_vec(),
        };
        if self.confirmed(akey, &repo)
            && let Some((_, entries)) = lock(&self.trees).iter().find(|(k, _)| *k == key)
        {
            return Ok(entries.clone());
        }
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            inc(&self.counters.upstream_requests, 1);
            let req = v1::ListTreeRequest {
                repo: Some(repo_ref.clone()),
                at: Some(v1::Revision { expr: at.to_text() }),
                path: prefix.to_vec(),
                recursive: true,
                page: Some(v1::PageRequest {
                    page_size: 1000,
                    page_token: token.clone(),
                }),
            };
            let reply = with_retries(self.cfg.max_retries, || {
                self.upstream.list_tree(auth, req.clone())
            })
            .await?;
            self.observe(&repo, reply.epochs.as_ref());
            out.extend(reply.msg.entries);
            token = reply
                .msg
                .page
                .map(|p| p.next_page_token)
                .unwrap_or_default();
            if token.is_empty() {
                break;
            }
        }
        self.confirm(akey, &repo, &[]);
        let entries = Arc::new(out);
        let mut trees = lock(&self.trees);
        trees.retain(|(k, _)| *k != key);
        if trees.len() >= TREE_CACHE_ENTRIES {
            trees.remove(0);
        }
        trees.push((key, entries.clone()));
        Ok(entries)
    }

    /// The changeset a revision expression names (a full changeset id is used as is).
    pub async fn resolve_revision(
        &self,
        auth: &Auth,
        repo_ref: &v1::RepoRef,
        expr: &str,
    ) -> Result<ObjectId> {
        if let Ok(id) = ObjectId::parse(expr)
            && id.kind() == ObjectKind::Changeset
        {
            return Ok(id);
        }
        let repo = repo_key(Some(repo_ref))?;
        inc(&self.counters.upstream_requests, 1);
        let req = v1::ResolveRequest {
            repo: Some(repo_ref.clone()),
            revisions: vec![v1::Revision {
                expr: expr.to_owned(),
            }],
        };
        let reply = with_retries(self.cfg.max_retries, || {
            self.upstream.resolve(auth, req.clone())
        })
        .await?;
        self.observe(&repo, reply.epochs.as_ref());
        let status = reply.msg.statuses.first().copied().unwrap_or(0);
        match reply.msg.ids.first() {
            Some(id) if status == v1::ItemStatus::Ok as i32 => ObjectId::from_bytes(&id.raw)
                .map_err(|e| err(ErrorReason::Internal, format!("bad id from upstream: {e}"))),
            _ => Err(err(
                ErrorReason::NotFound,
                format!("revision {expr:?} does not resolve"),
            )),
        }
    }

    // ----- info and stats ----------------------------------------------------------------

    /// `GetServerInfo` of the tier above, marked with this tier's name.
    pub async fn server_info(&self) -> Result<v1::ServerInfo> {
        if let Some((at, info)) = lock(&self.info).as_ref()
            && at.elapsed() < Duration::from_secs(5)
        {
            return Ok(info.clone());
        }
        inc(&self.counters.upstream_requests, 1);
        let mut info = with_retries(self.cfg.max_retries, || self.upstream.server_info()).await?;
        info.tier = self.cfg.kind.info_name().to_owned();
        if !info.features.iter().any(|f| f == "tier_cache") {
            info.features.push("tier_cache".into());
        }
        *lock(&self.info) = Some((Instant::now(), info.clone()));
        Ok(info)
    }

    /// `GetServerInfo` naming a repo: always asked of the tier above with the caller's
    /// credential (the repo's epochs are a read of that repo), marked with this tier's name.
    pub async fn repo_server_info(
        &self,
        auth: &Auth,
        req: v1::GetServerInfoRequest,
    ) -> Result<(v1::ServerInfo, Option<Epochs>)> {
        let repo = repo_key(req.repo.as_ref())?;
        inc(&self.counters.upstream_requests, 1);
        let reply = self.upstream.repo_server_info(auth, req).await?;
        self.observe(&repo, reply.epochs.as_ref());
        let mut info = reply.msg;
        info.tier = self.cfg.kind.info_name().to_owned();
        if !info.features.iter().any(|f| f == "tier_cache") {
            info.features.push("tier_cache".into());
        }
        Ok((info, reply.epochs))
    }

    /// Counters as JSON.
    pub fn stats_view(&self) -> StatsView {
        let c = &self.counters;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let (hits, misses) = (g(&c.hits), g(&c.misses));
        StatsView {
            tier: self.cfg.kind.stats_name(),
            hits,
            misses,
            hit_rate: if hits + misses == 0 {
                0.0
            } else {
                hits as f64 / (hits + misses) as f64
            },
            fail_closed: g(&c.fail_closed),
            bytes_served: g(&c.bytes_served),
            upstream_requests: g(&c.upstream_requests),
            upstream_objects: g(&c.upstream_objects),
            decision_checks: g(&c.decision_checks),
            signed_url_fetches: g(&c.signed_url_fetches),
            signed_url_fallbacks: g(&c.signed_url_fallbacks),
            integrity_failures: g(&c.integrity_failures),
            ref_hits: g(&c.ref_hits),
            ref_misses: g(&c.ref_misses),
            forwarded: g(&c.forwarded),
            store: self.store.stats(),
        }
    }

    /// `GetTierStats`.
    pub fn tier_stats(&self) -> v1::TierStats {
        let s = self.stats_view();
        let (policy, redaction) = lock(&self.epochs)
            .values()
            .next()
            .map(|e| (e.policy.clone(), e.redaction.parse().unwrap_or(0)))
            .unwrap_or_default();
        v1::TierStats {
            tier: s.tier.to_owned(),
            hits: s.hits,
            misses: s.misses,
            fail_closed: s.fail_closed,
            bytes_served: s.bytes_served,
            upstream_requests: s.upstream_requests,
            policy_epoch: policy,
            redaction_epoch: redaction,
            started_at: Some(pbjson_types::Timestamp {
                seconds: self.started_ms / 1000,
                nanos: ((self.started_ms % 1000) * 1_000_000) as i32,
            }),
            cache_bytes: s.store.bytes,
        }
    }
}

/// Appends the chunks the server would send for one served object, honouring the request's
/// resume offset and length the way the server does.
fn push_chunks(
    out: &mut Vec<v1::ObjectChunk>,
    id: &ObjectId,
    bytes: &[u8],
    class: i32,
    resume_offset: u64,
    length: u64,
) {
    let total = bytes.len() as u64;
    let start = resume_offset.min(total) as usize;
    let end = if length > 0 {
        (start as u64 + length).min(total) as usize
    } else {
        total as usize
    };
    let mut pos = start;
    loop {
        let next = (pos + CHUNK).min(end);
        out.push(v1::ObjectChunk {
            id: Some(pid(id)),
            offset: pos as u64,
            data: bytes[pos..next].to_vec(),
            last: next >= end,
            status: v1::ItemStatus::Ok as i32,
            access_class: class,
            encoding: v1::ObjectEncoding::Raw as i32,
            total_size: total,
            ..Default::default()
        });
        pos = next;
        if pos >= end {
            break;
        }
    }
}
