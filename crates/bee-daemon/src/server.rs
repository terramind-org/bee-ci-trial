//! The HTTP surface of a tier, on a unix socket (node daemon) or TCP (regional cache).
//!
//! | Path | Served by |
//! |---|---|
//! | `POST /bee.v1.ObjectService/*` | the tier (Connect JSON or binary, via the `bee-proto` gateway) |
//! | `POST /bee.v1.*/*` (anything else) | forwarded byte for byte to the tier above, credential included |
//! | `POST /daemon/v1/<op>` | daemon-local operations (JSON) |
//! | `GET /healthz`, `GET /metrics` | health and Prometheus metrics |
//!
//! The daemon socket is open to every local user (`0666`): object RPCs are authorized by the
//! caller's token like any other read. Operations that touch the local file system (snapshots,
//! watching, pinning) are only accepted from the user the daemon runs as (peer credentials).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::{Request, State};
use axum::routing::{get, post};
use axum::serve::IncomingStream;
use bee_proto::connect::ConnectGateway;
use bee_proto::v1::object_service_server::ObjectServiceServer;
use bee_proto::v1::{ErrorDetail, ErrorReason};
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::error::{Result, err, http_status, invalid, io};
use crate::service::TierService;
use crate::snapshot::{self, SnapshotReport, SnapshotRequest};
use crate::tier::Tier;
use crate::upstream::Auth;
use crate::watch::WatchRegistry;

/// Who is on the other end of a connection.
#[derive(Clone, Debug, Default)]
pub struct Peer {
    /// Peer user id on a unix socket; `None` over TCP.
    pub uid: Option<u32>,
}

#[cfg(unix)]
impl Connected<IncomingStream<'_, tokio::net::UnixListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        Peer {
            uid: stream.io().peer_cred().ok().map(|c| c.uid()),
        }
    }
}

impl Connected<IncomingStream<'_, tokio::net::TcpListener>> for Peer {
    fn connect_info(_stream: IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Peer { uid: None }
    }
}

/// Shared state of a running tier.
pub struct App {
    /// The tier.
    pub tier: Arc<Tier>,
    /// Watchers.
    pub watches: WatchRegistry,
    /// The user allowed to run local file-system operations (`None`: nobody).
    pub owner_uid: Option<u32>,
}

/// A Connect-style JSON error response for `detail`.
pub fn error_response(detail: &ErrorDetail) -> Response<Body> {
    let status =
        StatusCode::from_u16(http_status(detail)).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let ce = bee_proto::connect::ConnectError::from_status(&crate::error::to_status(detail));
    let mut body = serde_json::to_value(&ce).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut body {
        map.insert(
            "error".into(),
            serde_json::to_value(detail).unwrap_or(Value::Null),
        );
    }
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap_or_default()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn json_response(v: &impl serde::Serialize) -> Response<Body> {
    match serde_json::to_vec(v) {
        Ok(b) => Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(b))
            .unwrap_or_else(|_| Response::new(Body::empty())),
        Err(e) => error_response(&err(ErrorReason::Internal, e.to_string())),
    }
}

/// The router of a tier.
pub fn router(app: Arc<App>) -> Router {
    let gateway = ConnectGateway::new()
        .max_message_bytes(crate::client::MAX_MESSAGE_BYTES)
        .add_service(ObjectServiceServer::new(TierService::new(app.tier.clone())))
        .into_router();
    Router::new()
        .route_service("/bee.v1.ObjectService/{method}", gateway)
        .route("/daemon/v1/{op}", post(local))
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/metrics", get(metrics))
        .fallback(forward)
        .with_state(app)
}

async fn forward(State(app): State<Arc<App>>, req: Request) -> Response<Body> {
    app.tier
        .counters
        .forwarded
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    app.tier.upstream().forward(req).await
}

async fn metrics(State(app): State<Arc<App>>) -> Response<Body> {
    let s = app.tier.stats_view();
    let t = s.tier;
    let mut out = String::new();
    let mut line = |name: &str, help: &str, kind: &str, v: f64| {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name}{{tier=\"{t}\"}} {v}\n"
        ));
    };
    line(
        "bee_cache_hits_total",
        "Objects served from the cache.",
        "counter",
        s.hits as f64,
    );
    line(
        "bee_cache_misses_total",
        "Objects fetched from the tier above.",
        "counter",
        s.misses as f64,
    );
    line(
        "bee_cache_hit_ratio",
        "Hits over objects served.",
        "gauge",
        s.hit_rate,
    );
    line(
        "bee_cache_fail_closed_total",
        "Reads refused because decisions could not be confirmed.",
        "counter",
        s.fail_closed as f64,
    );
    line(
        "bee_cache_bytes_served_total",
        "Object bytes served.",
        "counter",
        s.bytes_served as f64,
    );
    line(
        "bee_cache_upstream_requests_total",
        "Requests sent to the tier above.",
        "counter",
        s.upstream_requests as f64,
    );
    line(
        "bee_cache_signed_url_fetches_total",
        "Objects downloaded by signed URL.",
        "counter",
        s.signed_url_fetches as f64,
    );
    line(
        "bee_cache_signed_url_fallbacks_total",
        "Signed URLs that fell back to the API.",
        "counter",
        s.signed_url_fallbacks as f64,
    );
    line(
        "bee_cache_forwarded_total",
        "Requests forwarded byte for byte.",
        "counter",
        s.forwarded as f64,
    );
    line(
        "bee_cache_objects",
        "Objects held.",
        "gauge",
        s.store.objects as f64,
    );
    line(
        "bee_cache_bytes",
        "Bytes held.",
        "gauge",
        s.store.bytes as f64,
    );
    line(
        "bee_cache_evictions_total",
        "Objects evicted.",
        "counter",
        s.store.evictions as f64,
    );
    Response::builder()
        .header(CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(out))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PinRequest {
    name: String,
    #[serde(flatten)]
    target: SnapshotRequest,
}

#[derive(Deserialize)]
struct NameRequest {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RootRequest {
    root: PathBuf,
    #[serde(default)]
    since_ns: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapRequest {
    repo: String,
    #[serde(default)]
    sparse: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TreeRequest {
    repo: String,
    revision: String,
    #[serde(default)]
    prefix: String,
}

fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T> {
    let body: &[u8] = if body.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        body
    };
    serde_json::from_slice(body).map_err(|e| invalid("bad_json", e.to_string()))
}

const LOCAL_OPS: &[&str] = &[
    "Snapshot", "Watch", "Unwatch", "Changes", "Pin", "Unpin", "Flush",
];

async fn local(
    State(app): State<Arc<App>>,
    axum::extract::Path(op): axum::extract::Path<String>,
    req: Request,
) -> Response<Body> {
    let peer = req
        .extensions()
        .get::<ConnectInfo<Peer>>()
        .map(|c| c.0.clone())
        .unwrap_or_default();
    let auth = Auth(
        req.headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    );
    if LOCAL_OPS.contains(&op.as_str()) && (peer.uid.is_none() || peer.uid != app.owner_uid) {
        return error_response(&err(
            ErrorReason::PermissionDenied,
            format!("{op} is only available to the daemon's own user over its socket"),
        ));
    }
    let body = match axum::body::to_bytes(req.into_body(), 1 << 20).await {
        Ok(b) => b,
        Err(e) => return error_response(&invalid("bad_body", e.to_string())),
    };
    match run_local(&app, &op, &auth, &body).await {
        Ok(v) => json_response(&v),
        Err(e) => error_response(&e),
    }
}

async fn run_local(app: &Arc<App>, op: &str, auth: &Auth, body: &Bytes) -> Result<Value> {
    let tier = &app.tier;
    match op {
        "Stats" => Ok(serde_json::to_value(tier.stats_view()).unwrap_or(Value::Null)),
        "Bootstrap" => {
            let req: BootstrapRequest = parse(body)?;
            let report = crate::bootstrap::bootstrap(tier, auth, &req.repo, &req.sparse).await?;
            Ok(serde_json::to_value(report).unwrap_or(Value::Null))
        }
        "Prefetch" => {
            let req: SnapshotRequest = parse(body)?;
            let mut report = SnapshotReport::default();
            let leaves = snapshot::prepare(tier, auth, &req, &mut report).await?;
            report.files = leaves.len() as u64;
            report.bytes = leaves.iter().map(|l| l.size).sum();
            Ok(serde_json::to_value(report).unwrap_or(Value::Null))
        }
        "Snapshot" => {
            let req: SnapshotRequest = parse(body)?;
            let report = snapshot::snapshot(tier.clone(), auth, req).await?;
            Ok(serde_json::to_value(report).unwrap_or(Value::Null))
        }
        "Tree" => {
            let req: TreeRequest = parse(body)?;
            let repo = snapshot::repo_ref(&req.repo);
            let at = tier.resolve_revision(auth, &repo, &req.revision).await?;
            let (leaves, skipped) = snapshot::leaves(
                tier,
                auth,
                &repo,
                &at,
                std::slice::from_ref(&req.prefix),
                &[],
            )
            .await?;
            let entries: Vec<Value> = leaves
                .iter()
                .map(|l| {
                    json!({
                        "path": String::from_utf8_lossy(&l.path),
                        "mode": l.mode.as_u64(),
                        "id": l.id.to_text(),
                        "size": l.size,
                    })
                })
                .collect();
            Ok(json!({ "changeset": at.to_text(), "entries": entries, "skipped": skipped }))
        }
        "Pin" => {
            let req: PinRequest = parse(body)?;
            if req.name.is_empty() {
                return Err(invalid("missing_field", "name is required"));
            }
            let mut report = SnapshotReport::default();
            let leaves = snapshot::prepare(tier, auth, &req.target, &mut report).await?;
            let ids: Vec<_> = leaves.iter().map(|l| l.id).collect();
            tier.store().pin_set(&req.name, &ids)?;
            Ok(
                json!({ "name": req.name, "changeset": report.changeset, "objects": ids.len(), "skipped": report.skipped }),
            )
        }
        "Unpin" => {
            let req: NameRequest = parse(body)?;
            Ok(json!({ "removed": tier.store().unpin_set(&req.name)? }))
        }
        "Pins" => {
            let sets: Vec<Value> = tier
                .store()
                .pin_sets()
                .into_iter()
                .map(|(n, c)| json!({ "name": n, "objects": c }))
                .collect();
            Ok(json!({ "pins": sets }))
        }
        "Flush" => {
            tier.store().flush()?;
            Ok(json!({}))
        }
        "Watch" => {
            let req: RootRequest = parse(body)?;
            let root = req.root.clone();
            let started = run_blocking(app, move |a| a.watches.watch(&root)).await?;
            Ok(json!({ "root": req.root, "watchingSinceNs": started }))
        }
        "Unwatch" => {
            let req: RootRequest = parse(body)?;
            Ok(json!({ "removed": app.watches.unwatch(&req.root) }))
        }
        "Changes" => {
            let req: RootRequest = parse(body)?;
            let root = req.root.clone();
            let since = req.since_ns;
            let changes = run_blocking(app, move |a| a.watches.changes(&root, since)).await?;
            Ok(serde_json::to_value(changes).unwrap_or(Value::Null))
        }
        _ => Err(err(
            ErrorReason::Unimplemented,
            format!("unknown daemon operation {op:?}"),
        )),
    }
}

async fn run_blocking<T: Send + 'static>(
    app: &Arc<App>,
    f: impl FnOnce(&App) -> Result<T> + Send + 'static,
) -> Result<T> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || f(&app))
        .await
        .map_err(|e| err(ErrorReason::Internal, format!("task failed: {e}")))?
}

/// Where a running tier listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listening {
    /// A unix socket.
    Unix(PathBuf),
    /// A TCP address (`http://` base URL).
    Tcp(String),
}

/// A running tier; dropping it does not stop it, [`Running::shutdown`] does.
pub struct Running {
    /// Where it listens.
    pub listening: Listening,
    /// Its state.
    pub app: Arc<App>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    flusher: Option<tokio::task::JoinHandle<()>>,
}

impl Running {
    /// The endpoint text a client connects to.
    pub fn endpoint(&self) -> String {
        match &self.listening {
            Listening::Unix(p) => format!("unix:{}", p.display()),
            Listening::Tcp(u) => u.clone(),
        }
    }

    /// Stops accepting, waits for the server task and persists the cache index.
    pub async fn shutdown(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
        if let Some(f) = self.flusher.take() {
            f.abort();
        }
        let _ = self.app.tier.store().flush();
        if let Listening::Unix(p) = &self.listening {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn spawn_flusher(app: &Arc<App>) -> tokio::task::JoinHandle<()> {
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let _ = app.tier.store().flush();
        }
    })
}

/// Serves `tier` on a unix socket at `path` (the node daemon). A stale socket is replaced;
/// a live one (another daemon) is refused with `ALREADY_EXISTS`.
#[cfg(unix)]
pub async fn serve_unix(tier: Arc<Tier>, path: &Path) -> Result<Running> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if path.exists() {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(err(
                ErrorReason::AlreadyExists,
                format!("a daemon is already listening on {}", path.display()),
            ));
        }
        std::fs::remove_file(path).map_err(|e| io(format!("remove {}", path.display()), &e))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| io(format!("create {}", parent.display()), &e))?;
    }
    let listener = tokio::net::UnixListener::bind(path)
        .map_err(|e| io(format!("bind {}", path.display()), &e))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| io(format!("chmod {}", path.display()), &e))?;
    let owner_uid = std::fs::metadata(path).ok().map(|m| m.uid());
    let app = Arc::new(App {
        tier,
        watches: WatchRegistry::new(),
        owner_uid,
    });
    let (stop, rx) = oneshot::channel::<()>();
    let svc = router(app.clone()).into_make_service_with_connect_info::<Peer>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, svc)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    let flusher = spawn_flusher(&app);
    Ok(Running {
        listening: Listening::Unix(path.to_path_buf()),
        app,
        stop: Some(stop),
        task: Some(task),
        flusher: Some(flusher),
    })
}

/// Serves `tier` over TCP at `addr` (the regional cache). Local file-system operations are
/// refused over TCP.
pub async fn serve_tcp(tier: Arc<Tier>, addr: &str) -> Result<Running> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| io(format!("bind {addr}"), &e))?;
    let local = listener
        .local_addr()
        .map_err(|e| io(format!("bind {addr}"), &e))?;
    let app = Arc::new(App {
        tier,
        watches: WatchRegistry::new(),
        owner_uid: None,
    });
    let (stop, rx) = oneshot::channel::<()>();
    let svc = router(app.clone()).into_make_service_with_connect_info::<Peer>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, svc)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    Ok(Running {
        listening: Listening::Tcp(format!("http://{local}")),
        app,
        stop: Some(stop),
        task: Some(task),
        flusher: None,
    })
}
