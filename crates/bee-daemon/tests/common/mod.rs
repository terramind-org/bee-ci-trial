//! Shared harness for the daemon's integration tests: an in-process `bee-server` (dev mode,
//! SQLite in a tempdir), a node daemon or regional cache in front of it, an upstream that
//! counts what it is asked, an in-memory upstream for tier tests, and the glue that runs a
//! working copy through the daemon (`bee_wc::Transport` over the daemon client).

#![allow(dead_code, missing_docs, clippy::type_complexity)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bee_core::{Mode, ObjectId, ObjectKind};
use bee_daemon::upstream::{Auth, Epochs, Reply};
use bee_daemon::{
    BlockingClient, DiskCache, DiskConfig, Endpoint, HttpUpstream, ObjectStore, Running, Tier,
    TierConfig, Upstream,
};
use bee_proto::v1::{self, ErrorDetail, ErrorReason};
use bee_wc::manifest::{self, Manifest};
use bee_wc::report::FileChange;
use bee_wc::scan::{self, StatCache};
use bee_wc::{CommitOptions, CreateOptions, OpenOptions, Signature, Sparse, WorkingCopy};
use bytes::Bytes;
use serde_json::{Value, json};

pub const REPO: &str = "dev/playground";

// ----- working copies through any Connect endpoint ------------------------------------------

/// `bee_wc::Transport` over the daemon's blocking client (unix socket or HTTP).
pub struct WcTransport(pub BlockingClient);

impl bee_wc::Transport for WcTransport {
    fn unary(&self, path: &str, request: &Value) -> Result<Value, Box<ErrorDetail>> {
        self.0.unary(path, request)
    }

    fn server_stream(&self, path: &str, request: &Value) -> Result<Vec<Value>, Box<ErrorDetail>> {
        self.0.server_stream(path, request)
    }

    fn client_stream(&self, path: &str, requests: &[Value]) -> Result<Value, Box<ErrorDetail>> {
        self.0.client_stream(path, requests)
    }
}

pub fn author() -> Signature {
    Signature {
        name: "tester".into(),
        time: 1_792_281_600,
        tz_offset_minutes: 0,
    }
}

pub fn commit_opts(message: &str) -> CommitOptions {
    CommitOptions {
        message: message.into(),
        author: author(),
        allow_empty: false,
        paths: Vec::new(),
        provenance: None,
    }
}

pub fn clone_wc(dir: &Path, t: &WcTransport, sparse: Sparse) -> WorkingCopy {
    let opts = CreateOptions {
        endpoint: None,
        repo: REPO.into(),
        target_ref: "main".into(),
        sparse,
        seed: None,
        adopt: false,
    };
    bee_wc::create(dir, &opts, t, &OpenOptions::default())
        .expect("clone")
        .0
}

/// Adopts a directory a daemon snapshot filled as a working copy of `main` (`bee init`):
/// files already in place are kept, nothing is fetched.
pub fn adopt_wc(dir: &Path, t: &WcTransport, sparse: Sparse) -> WorkingCopy {
    let opts = CreateOptions {
        endpoint: None,
        repo: REPO.into(),
        target_ref: "main".into(),
        sparse,
        seed: None,
        adopt: true,
    };
    bee_wc::create(dir, &opts, t, &OpenOptions::default())
        .expect("adopt")
        .0
}

/// Files of a working tree (excluding `.bee`), path to bytes.
pub fn tree_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let mut entries: Vec<_> = fs::read_dir(dir).expect("read").flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if dir == root && e.file_name() == ".bee" {
                continue;
            }
            let ft = e.file_type().expect("type");
            if ft.is_dir() {
                walk(root, &p, out);
            } else if ft.is_symlink() {
                let t = fs::read_link(&p).expect("readlink");
                out.insert(rel(root, &p), t.to_string_lossy().as_bytes().to_vec());
            } else {
                out.insert(rel(root, &p), fs::read(&p).expect("read"));
            }
        }
    }
    walk(root, root, &mut out);
    out
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .expect("under root")
        .to_string_lossy()
        .replace('\\', "/")
}

// ----- an in-process server ------------------------------------------------------------------

pub struct Server {
    pub rt: tokio::runtime::Runtime,
    pub srv: Option<bee_server::server::RunningServer>,
    pub token: String,
    pub url: String,
    pub user: String,
    pub dir: tempfile::TempDir,
}

pub fn start_server() -> Server {
    start_server_with(|_| {})
}

pub fn start_server_with(f: impl FnOnce(&mut bee_server::config::Config)) -> Server {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg =
        bee_server::config::Config::dev(dir.path().to_path_buf(), Some("127.0.0.1:0".into()));
    cfg.redaction.purge_interval_secs = 3600;
    // Debug builds on a shared, loaded machine: generous deadlines for large seed commits.
    cfg.limits.default_deadline_ms = 300_000;
    cfg.metadata.query_timeout_ms = 60_000;
    f(&mut cfg);
    let mut opts = bee_server::server::ServerOptions::new(cfg);
    opts.dev = true;
    let srv = rt
        .block_on(bee_server::server::start(opts))
        .expect("server starts");
    let dev = srv.dev.as_ref().expect("dev bootstrap");
    let (token, user) = (dev.token.clone(), dev.user.clone());
    let url = srv.url.clone();
    Server {
        rt,
        srv: Some(srv),
        token,
        url,
        user,
        dir,
    }
}

impl Server {
    pub fn client(&self, token: &str) -> BlockingClient {
        BlockingClient::new(Endpoint::Http(self.url.clone()), Some(token)).expect("client")
    }

    pub fn transport(&self) -> WcTransport {
        WcTransport(self.client(&self.token))
    }

    /// Lands `files` on `main` through a scratch working copy; returns the new tip.
    pub fn land_files(&self, files: &[(String, Vec<u8>)], message: &str) -> (String, u64) {
        let t = self.transport();
        let scratch = tempfile::tempdir().expect("scratch");
        let root = scratch.path().join("wc");
        let mut wc = clone_wc(&root, &t, Sparse::full());
        for (p, b) in files {
            let full = root.join(p);
            fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            fs::write(&full, b).expect("write");
        }
        wc.commit(&t, &commit_opts(message)).expect("commit");
        wc.land(&t).expect("land");
        self.tip()
    }

    pub fn tip(&self) -> (String, u64) {
        let r = self
            .client(&self.token)
            .unary(
                "/bee.v1.ObjectService/ResolveRef",
                &json!({ "repo": { "fullName": REPO }, "name": "main" }),
            )
            .expect("resolve");
        let id = id_from_json(&r["ref"]["target"]);
        let version = r["ref"]["version"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        (id.to_text(), version)
    }

    /// An agent token for a new agent principal.
    pub fn agent_token(&self, name: &str) -> String {
        let c = self.client(&self.token);
        let agent = c
            .unary(
                "/bee.v1.AuthService/CreateAgent",
                &json!({ "display": name }),
            )
            .expect("agent");
        let minted = c
            .unary(
                "/bee.v1.AuthService/MintToken",
                &json!({ "principalId": agent["id"] }),
            )
            .expect("mint");
        minted["token"].as_str().expect("token").to_owned()
    }

    pub fn http_upstream(&self) -> HttpUpstream {
        let _g = self.rt.enter();
        HttpUpstream::new(&self.url, Duration::from_secs(120)).expect("upstream")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(s) = self.srv.take() {
            self.rt.block_on(s.shutdown());
        }
    }
}

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn unb64(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .expect("base64")
}

pub fn id_json(id: &ObjectId) -> Value {
    json!({ "raw": b64(id.as_bytes()) })
}

pub fn id_from_json(v: &Value) -> ObjectId {
    ObjectId::from_bytes(&unb64(v["raw"].as_str().expect("raw"))).expect("id")
}

/// Concatenated object bytes per id from GetObjects JSON chunks.
pub fn objects_of(chunks: &[Value]) -> HashMap<ObjectId, Result<Vec<u8>, String>> {
    let mut out: HashMap<ObjectId, Result<Vec<u8>, String>> = HashMap::new();
    for c in chunks {
        let id = id_from_json(&c["id"]);
        let status = c["status"].as_str().unwrap_or("");
        if status != "ITEM_STATUS_OK" {
            out.insert(
                id,
                Err(c["errorReason"].as_str().unwrap_or(status).to_owned()),
            );
            continue;
        }
        let data = c["data"].as_str().map(unb64).unwrap_or_default();
        if let Ok(buf) = out.entry(id).or_insert_with(|| Ok(Vec::new())) {
            buf.extend(data);
        }
    }
    out
}

// ----- a daemon or regional cache in front of an upstream ------------------------------------

pub struct Daemon {
    pub running: Option<Running>,
    pub tier: Arc<Tier>,
    pub dir: tempfile::TempDir,
    pub endpoint: Endpoint,
}

pub fn short_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("swd")
        .tempdir_in("/tmp")
        .expect("tempdir")
}

pub fn disk_store(dir: &Path, max_bytes: u64) -> Arc<dyn ObjectStore> {
    let mut cfg = DiskConfig::new(dir);
    cfg.max_bytes = max_bytes;
    cfg.fsync = false;
    Arc::new(DiskCache::open(cfg).expect("cache"))
}

pub fn start_daemon(
    rt: &tokio::runtime::Runtime,
    upstream: Arc<dyn Upstream>,
    cfg: TierConfig,
) -> Daemon {
    let dir = short_tempdir();
    let store = disk_store(&dir.path().join("cache"), 4 << 30);
    let tier = Arc::new(Tier::new(cfg, store, upstream));
    let socket = dir.path().join("d.sock");
    let running = rt
        .block_on(bee_daemon::server::serve_unix(tier.clone(), &socket))
        .expect("daemon serves");
    Daemon {
        running: Some(running),
        tier,
        dir,
        endpoint: Endpoint::Unix(socket),
    }
}

pub fn start_regional(
    rt: &tokio::runtime::Runtime,
    upstream: Arc<dyn Upstream>,
    cfg: TierConfig,
) -> Daemon {
    let dir = short_tempdir();
    let store: Arc<dyn ObjectStore> = Arc::new(bee_daemon::MemoryCache::new(1 << 30, 90));
    let tier = Arc::new(Tier::new(cfg, store, upstream));
    let running = rt
        .block_on(bee_daemon::server::serve_tcp(tier.clone(), "127.0.0.1:0"))
        .expect("regional serves");
    let endpoint = Endpoint::parse(&running.endpoint()).expect("endpoint");
    Daemon {
        running: Some(running),
        tier,
        dir,
        endpoint,
    }
}

impl Daemon {
    pub fn client(&self, token: Option<&str>) -> BlockingClient {
        BlockingClient::new(self.endpoint.clone(), token).expect("client")
    }

    pub fn transport(&self, token: &str) -> WcTransport {
        WcTransport(self.client(Some(token)))
    }

    pub fn stop(&mut self, rt: &tokio::runtime::Runtime) {
        if let Some(r) = self.running.take() {
            rt.block_on(r.shutdown());
        }
    }
}

// ----- an upstream that counts ---------------------------------------------------------------

/// Wraps an upstream and counts what it is asked for.
pub struct Counting<U> {
    pub inner: U,
    pub fetched: Mutex<HashMap<ObjectId, u32>>,
    pub get_calls: AtomicU64,
    pub has_calls: AtomicU64,
    pub resolve_calls: AtomicU64,
}

impl<U> Counting<U> {
    pub fn new(inner: U) -> Self {
        Self {
            inner,
            fetched: Mutex::new(HashMap::new()),
            get_calls: AtomicU64::new(0),
            has_calls: AtomicU64::new(0),
            resolve_calls: AtomicU64::new(0),
        }
    }

    pub fn max_fetches_per_object(&self) -> u32 {
        self.fetched
            .lock()
            .expect("lock")
            .values()
            .copied()
            .max()
            .unwrap_or(0)
    }

    pub fn objects_fetched(&self) -> usize {
        self.fetched.lock().expect("lock").len()
    }
}

#[tonic::async_trait]
impl<U: Upstream> Upstream for Counting<U> {
    async fn get_objects(
        &self,
        auth: &Auth,
        req: v1::GetObjectsRequest,
    ) -> Result<Reply<Vec<v1::ObjectChunk>>, Box<ErrorDetail>> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        {
            let mut f = self.fetched.lock().expect("lock");
            for raw in &req.ids {
                if let Ok(id) = ObjectId::from_bytes(&raw.raw) {
                    *f.entry(id).or_insert(0) += 1;
                }
            }
        }
        self.inner.get_objects(auth, req).await
    }
    async fn has_objects(
        &self,
        auth: &Auth,
        req: v1::HasObjectsRequest,
    ) -> Result<Reply<v1::HasObjectsResponse>, Box<ErrorDetail>> {
        self.has_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.has_objects(auth, req).await
    }
    async fn resolve_ref(
        &self,
        auth: &Auth,
        req: v1::ResolveRefRequest,
    ) -> Result<Reply<v1::ResolveRefResponse>, Box<ErrorDetail>> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.resolve_ref(auth, req).await
    }
    async fn server_info(&self) -> Result<v1::ServerInfo, Box<ErrorDetail>> {
        self.inner.server_info().await
    }
    async fn daemon_snapshot(
        &self,
        auth: &Auth,
        req: v1::GetDaemonSnapshotRequest,
    ) -> Result<v1::DaemonSnapshot, Box<ErrorDetail>> {
        self.inner.daemon_snapshot(auth, req).await
    }
    async fn list_tree(
        &self,
        auth: &Auth,
        req: v1::ListTreeRequest,
    ) -> Result<Reply<v1::ListTreeResponse>, Box<ErrorDetail>> {
        self.inner.list_tree(auth, req).await
    }
    async fn resolve(
        &self,
        auth: &Auth,
        req: v1::ResolveRequest,
    ) -> Result<Reply<v1::ResolveResponse>, Box<ErrorDetail>> {
        self.inner.resolve(auth, req).await
    }
    async fn fetch_url(&self, url: &str) -> Result<Bytes, String> {
        self.inner.fetch_url(url).await
    }
    async fn forward(
        &self,
        req: http::Request<axum::body::Body>,
    ) -> http::Response<axum::body::Body> {
        self.inner.forward(req).await
    }
}

// ----- an in-memory upstream for tier tests --------------------------------------------------

/// A fake tier above: one repo, objects readable per token, one changeset tree.
pub struct MockUpstream {
    pub objects: Mutex<HashMap<ObjectId, Vec<u8>>>,
    /// Token header -> readable object ids (`None` entry = everything).
    pub readable: Mutex<HashMap<String, Option<HashSet<ObjectId>>>>,
    pub redacted: Mutex<HashSet<ObjectId>>,
    pub tree: Mutex<Vec<v1::TreeEntry>>,
    pub changeset: ObjectId,
    pub epochs: Mutex<Epochs>,
    pub down: AtomicBool,
    pub delay_ms: AtomicU64,
    pub signed_min: AtomicU64,
    pub cdn_down: AtomicBool,
    pub corrupt_once: Mutex<HashSet<ObjectId>>,
    pub get_calls: AtomicU64,
    pub has_calls: AtomicU64,
    pub url_calls: AtomicU64,
    pub ref_version: AtomicU64,
    /// `GetObjects` calls in flight now, and the most ever in flight at once.
    pub inflight: AtomicU64,
    pub max_inflight: AtomicU64,
}

impl MockUpstream {
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
            readable: Mutex::new(HashMap::new()),
            redacted: Mutex::new(HashSet::new()),
            tree: Mutex::new(Vec::new()),
            changeset: ObjectId::new(ObjectKind::Changeset, [9u8; 32]),
            epochs: Mutex::new(Epochs {
                policy: "1".into(),
                redaction: "1".into(),
            }),
            down: AtomicBool::new(false),
            delay_ms: AtomicU64::new(0),
            signed_min: AtomicU64::new(u64::MAX),
            cdn_down: AtomicBool::new(false),
            corrupt_once: Mutex::new(HashSet::new()),
            get_calls: AtomicU64::new(0),
            has_calls: AtomicU64::new(0),
            url_calls: AtomicU64::new(0),
            ref_version: AtomicU64::new(1),
            inflight: AtomicU64::new(0),
            max_inflight: AtomicU64::new(0),
        }
    }

    pub fn add(&self, path: &str, bytes: &[u8], mode: Mode) -> ObjectId {
        let id = bee_object::blob_id(bytes);
        self.objects
            .lock()
            .expect("lock")
            .insert(id, bytes.to_vec());
        let pmode = match mode {
            Mode::Executable => v1::Mode::Executable,
            Mode::Symlink => v1::Mode::Symlink,
            _ => v1::Mode::File,
        };
        self.tree.lock().expect("lock").push(v1::TreeEntry {
            path: path.as_bytes().to_vec(),
            mode: pmode as i32,
            id: Some(v1::ObjectId {
                raw: id.as_bytes().to_vec(),
            }),
            size: bytes.len() as u64,
            restricted: false,
        });
        id
    }

    pub fn allow_all(&self, token: &str) {
        self.readable
            .lock()
            .expect("lock")
            .insert(format!("Bearer {token}"), None);
    }

    pub fn allow(&self, token: &str, ids: &[ObjectId]) {
        self.readable.lock().expect("lock").insert(
            format!("Bearer {token}"),
            Some(ids.iter().copied().collect()),
        );
    }

    fn gate(&self, auth: &Auth) -> Result<Option<HashSet<ObjectId>>, Box<ErrorDetail>> {
        if self.down.load(Ordering::SeqCst) {
            return Err(bee_daemon::error::unavailable("upstream down"));
        }
        let key = auth.header().unwrap_or("").to_owned();
        match self.readable.lock().expect("lock").get(&key) {
            Some(r) => Ok(r.clone()),
            None => Err(bee_daemon::error::err(
                ErrorReason::Unauthenticated,
                "no credential",
            )),
        }
    }

    fn readable_by(r: &Option<HashSet<ObjectId>>, id: &ObjectId) -> bool {
        r.as_ref().is_none_or(|s| s.contains(id))
    }

    fn epochs(&self) -> Option<Epochs> {
        Some(self.epochs.lock().expect("lock").clone())
    }
}

#[tonic::async_trait]
impl Upstream for MockUpstream {
    async fn get_objects(
        &self,
        auth: &Auth,
        req: v1::GetObjectsRequest,
    ) -> Result<Reply<Vec<v1::ObjectChunk>>, Box<ErrorDetail>> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_inflight.fetch_max(now, Ordering::SeqCst);
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        let r = self.gate(auth)?;
        let mut out = Vec::new();
        for raw in &req.ids {
            let id = ObjectId::from_bytes(&raw.raw).expect("id");
            let status = |s: v1::ItemStatus, reason: &str| v1::ObjectChunk {
                id: Some(raw.clone()),
                last: true,
                status: s as i32,
                error_reason: reason.into(),
                ..Default::default()
            };
            let bytes = self.objects.lock().expect("lock").get(&id).cloned();
            let Some(mut bytes) = bytes.filter(|_| Self::readable_by(&r, &id)) else {
                out.push(status(v1::ItemStatus::NotFound, "NOT_FOUND"));
                continue;
            };
            if self.redacted.lock().expect("lock").contains(&id) {
                out.push(status(v1::ItemStatus::Redacted, "REDACTED"));
                continue;
            }
            if req.allow_signed_urls && bytes.len() as u64 >= self.signed_min.load(Ordering::SeqCst)
            {
                out.push(v1::ObjectChunk {
                    id: Some(raw.clone()),
                    last: true,
                    signed_url: format!("mock://{}", id.to_text()),
                    status: v1::ItemStatus::Ok as i32,
                    access_class: v1::AccessClass::Root as i32,
                    encoding: v1::ObjectEncoding::Raw as i32,
                    total_size: bytes.len() as u64,
                    ..Default::default()
                });
                continue;
            }
            if self.corrupt_once.lock().expect("lock").remove(&id) && !bytes.is_empty() {
                bytes[0] ^= 0xff;
            }
            out.push(v1::ObjectChunk {
                id: Some(raw.clone()),
                offset: 0,
                total_size: bytes.len() as u64,
                data: bytes,
                last: true,
                status: v1::ItemStatus::Ok as i32,
                access_class: v1::AccessClass::Root as i32,
                encoding: v1::ObjectEncoding::Raw as i32,
                ..Default::default()
            });
        }
        Ok(Reply {
            msg: out,
            epochs: self.epochs(),
        })
    }

    async fn has_objects(
        &self,
        auth: &Auth,
        req: v1::HasObjectsRequest,
    ) -> Result<Reply<v1::HasObjectsResponse>, Box<ErrorDetail>> {
        self.has_calls.fetch_add(1, Ordering::SeqCst);
        let r = self.gate(auth)?;
        let objects = self.objects.lock().expect("lock");
        let redacted = self.redacted.lock().expect("lock");
        let present = req
            .ids
            .iter()
            .map(|raw| {
                let id = ObjectId::from_bytes(&raw.raw).expect("id");
                objects.contains_key(&id) && !redacted.contains(&id) && Self::readable_by(&r, &id)
            })
            .collect();
        Ok(Reply {
            msg: v1::HasObjectsResponse { present },
            epochs: self.epochs(),
        })
    }

    async fn resolve_ref(
        &self,
        auth: &Auth,
        req: v1::ResolveRefRequest,
    ) -> Result<Reply<v1::ResolveRefResponse>, Box<ErrorDetail>> {
        self.gate(auth)?;
        let version = self.ref_version.load(Ordering::SeqCst).max(req.min_version);
        Ok(Reply {
            msg: v1::ResolveRefResponse {
                r#ref: Some(v1::Ref {
                    name: req.name,
                    target: Some(v1::ObjectId {
                        raw: self.changeset.as_bytes().to_vec(),
                    }),
                    version,
                }),
                root_tree: None,
                stale: false,
                renamed_to: String::new(),
            },
            epochs: self.epochs(),
        })
    }

    async fn server_info(&self) -> Result<v1::ServerInfo, Box<ErrorDetail>> {
        Ok(v1::ServerInfo {
            tier: "server".into(),
            ..Default::default()
        })
    }

    async fn daemon_snapshot(
        &self,
        _auth: &Auth,
        _req: v1::GetDaemonSnapshotRequest,
    ) -> Result<v1::DaemonSnapshot, Box<ErrorDetail>> {
        Err(bee_daemon::error::err(
            ErrorReason::Unimplemented,
            "no snapshots",
        ))
    }

    async fn list_tree(
        &self,
        auth: &Auth,
        req: v1::ListTreeRequest,
    ) -> Result<Reply<v1::ListTreeResponse>, Box<ErrorDetail>> {
        self.gate(auth)?;
        let tree = self.tree.lock().expect("lock");
        let prefix = req.path.clone();
        let matching: Vec<v1::TreeEntry> = tree
            .iter()
            .filter(|e| {
                prefix.is_empty()
                    || e.path == prefix
                    || (e.path.starts_with(&prefix) && e.path.get(prefix.len()) == Some(&b'/'))
            })
            .cloned()
            .collect();
        let page = req.page.unwrap_or_default();
        let start: usize = page.page_token.parse().unwrap_or(0);
        let size = if page.page_size == 0 {
            1000
        } else {
            page.page_size as usize
        };
        let end = (start + size).min(matching.len());
        let next = if end < matching.len() {
            end.to_string()
        } else {
            String::new()
        };
        Ok(Reply {
            msg: v1::ListTreeResponse {
                entries: matching[start..end].to_vec(),
                page: Some(v1::PageResponse {
                    next_page_token: next,
                }),
            },
            epochs: self.epochs(),
        })
    }

    async fn resolve(
        &self,
        auth: &Auth,
        _req: v1::ResolveRequest,
    ) -> Result<Reply<v1::ResolveResponse>, Box<ErrorDetail>> {
        self.gate(auth)?;
        Ok(Reply {
            msg: v1::ResolveResponse {
                ids: vec![v1::ObjectId {
                    raw: self.changeset.as_bytes().to_vec(),
                }],
                statuses: vec![v1::ItemStatus::Ok as i32],
            },
            epochs: self.epochs(),
        })
    }

    async fn fetch_url(&self, url: &str) -> Result<Bytes, String> {
        self.url_calls.fetch_add(1, Ordering::SeqCst);
        if self.cdn_down.load(Ordering::SeqCst) {
            return Err("cdn unreachable".into());
        }
        let text = url.strip_prefix("mock://").ok_or("bad url")?;
        let id = ObjectId::parse(text).map_err(|e| e.to_string())?;
        self.objects
            .lock()
            .expect("lock")
            .get(&id)
            .cloned()
            .map(Bytes::from)
            .ok_or_else(|| "missing".to_owned())
    }
}

// ----- watcher-driven status (the composition the CLI performs) -------------------------------

/// Status computed from the daemon's watcher journal instead of a walk: ask the daemon which
/// paths changed since the stat cache's clock, rescan only those with `bee_wc::scan`, merge
/// with the stat cache, and save the refreshed cache. Falls back to `WorkingCopy::status` when
/// the daemon answers `fresh`. Returns the changes and whether the watcher was used.
pub fn watched_status(
    root: &Path,
    daemon: &BlockingClient,
    t: &dyn bee_wc::Transport,
) -> (Vec<FileChange>, bool, usize) {
    let mut wc = WorkingCopy::open(root, t, &OpenOptions::default()).expect("open");
    let bee = root.join(".bee");
    let cache = fs::read(bee.join("statcache"))
        .ok()
        .and_then(|b| StatCache::decode(&b).ok())
        .unwrap_or_default();
    let resp = daemon
        .local(
            "Changes",
            &json!({ "root": root, "sinceNs": cache.clock_ns }),
        )
        .expect("changes");
    if resp["fresh"].as_bool().unwrap_or(true) {
        let s = wc.status().expect("status");
        return (s.changes, false, usize::MAX);
    }
    let state = wc.state().clone();
    let base = wc.manifest().clone();
    let mut dirty: Vec<String> = Vec::new();
    for p in resp["paths"].as_array().cloned().unwrap_or_default() {
        let p = p.as_str().unwrap_or_default().to_owned();
        // A .gitignore change can hide or reveal anything in its directory.
        let p = match p.strip_suffix(".gitignore") {
            Some(dir) if dir.is_empty() || dir.ends_with('/') => {
                dir.trim_end_matches('/').to_owned()
            }
            _ => p,
        };
        if p.is_empty() {
            dirty = vec![String::new()];
            break;
        }
        if bee_core::RepoPath::new(p.as_bytes().to_vec()).is_ok() {
            dirty.push(p);
        }
    }
    let under = |path: &[u8]| {
        dirty.iter().any(|d| {
            d.is_empty()
                || path == d.as_bytes()
                || (path.starts_with(d.as_bytes()) && path.get(d.len()) == Some(&b'/'))
        })
    };
    let clock = scan::touch_clock(&bee).expect("clock");
    let mut seed = cache.clone();
    for d in &dirty {
        seed.entries.remove(d.as_bytes());
    }
    let mut entries = cache.entries.clone();
    entries.retain(|p, _| !under(p));
    let mut actual: Manifest = Manifest::new();
    if !dirty.is_empty() {
        let profile = Sparse::from_prefixes(dirty.iter()).expect("profile");
        let partial =
            scan::scan(root, clock, &profile, &base, &state.hidden_set(), &seed).expect("scan");
        for (p, e) in partial.cache.entries {
            if state.sparse.contains(&p) {
                entries.insert(p, e);
            }
        }
    }
    for (p, (_, e)) in &entries {
        actual.insert(p.clone(), *e);
    }
    let hidden = state.hidden_set();
    for (p, e) in &base {
        if (hidden.contains(p) || e.mode == Mode::Gitlink) && !actual.contains_key(p) {
            actual.insert(p.clone(), *e);
        }
    }
    let refreshed = StatCache {
        clock_ns: clock,
        entries,
    };
    bee_wc::fsutil::atomic_write(&bee.join("statcache"), &refreshed.encode()).expect("save");
    drop(wc);
    let changes = bee_wc::report::file_changes(&manifest::diff(&base, &actual));
    (changes, true, dirty.len())
}

/// Changes from a full rehash of the working tree, writing nothing.
pub fn full_rehash(root: &Path, t: &dyn bee_wc::Transport) -> Vec<FileChange> {
    let wc = WorkingCopy::open(root, t, &OpenOptions::default()).expect("open");
    let state = wc.state().clone();
    let base = wc.manifest().clone();
    let s = scan::scan(
        root,
        i64::MAX,
        &state.sparse,
        &base,
        &state.hidden_set(),
        &StatCache::default(),
    )
    .expect("scan");
    bee_wc::report::file_changes(&manifest::diff(&base, &s.actual))
}

/// A transport for local-only operations (opening a working copy with no pending journal).
pub struct NoNetwork;

impl bee_wc::Transport for NoNetwork {
    fn unary(&self, _: &str, _: &Value) -> Result<Value, Box<ErrorDetail>> {
        Err(bee_daemon::error::unavailable("offline"))
    }
    fn server_stream(&self, _: &str, _: &Value) -> Result<Vec<Value>, Box<ErrorDetail>> {
        Err(bee_daemon::error::unavailable("offline"))
    }
    fn client_stream(&self, _: &str, _: &[Value]) -> Result<Value, Box<ErrorDetail>> {
        Err(bee_daemon::error::unavailable("offline"))
    }
}

pub fn path_buf(p: &str) -> PathBuf {
    PathBuf::from(p)
}
