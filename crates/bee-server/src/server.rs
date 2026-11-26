//! Startup: stores, state, routers, the single listener, dev-mode bootstrap and background
//! tasks.
//!
//! One TCP listener serves HTTP/1.1 and HTTP/2 (cleartext, prior knowledge or upgrade-free).
//! Requests whose `Content-Type` starts with `application/grpc` go to the tonic router; all
//! others go to the HTTP router: the Connect gateway (`POST /bee.v1.<S>/<M>`), `/mcp`,
//! `/healthz`, `/readyz`, `/metrics`, the signed-URL route `/blob/<org>/<id>` and git smart HTTP
//! plus LFS under `/<owner>/<repo>.git/` ([`crate::git_mount`]).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bee_core::{ChangeId, RefName};
use bee_object::{Changeset, Meta, Tree};
use bee_store::{
    BlobStore, Clock, Expected, FaultInjector, FaultyBlob, FaultyMeta, MetaStore, OrgId, OrgRow,
    PutOutcome, RefUpdate, RepoId, RepoRow, SystemClock,
};
use bee_store_blob::{
    DiskBlobConfig, DiskBlobStore, LocalUrlIssuer, S3BlobStore, S3Config, S3Credentials,
    S3UrlIssuer, SignedUrlIssuer,
};
use bee_store_sql::{PgMetaStore, PgOptions, SqliteMetaStore, SqliteOptions};
use http::{StatusCode, header};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower::ServiceExt;

use crate::audit::{AuditSink, KvAudit};
use crate::auth::{
    NS_ORG_ADMIN, NS_PRINCIPAL, NS_SERVICE, PrincipalRecord, ServiceRecord, TokenRecord,
};
use crate::config::Config;
use crate::errors::{ApiError, ApiResult};
use crate::kv::{Kv, PgKv, SqliteKv};
use crate::logging::{self, Level};
use crate::metrics::Metrics;
use crate::services::ServiceRegistry;
use crate::state::{AppState, ForgeInputs, StateParts};

/// A hook that registers extra services (forge areas) on the registry.
pub type ExtraServices = Box<dyn FnOnce(&mut ServiceRegistry) + Send>;

/// How to start a server.
pub struct ServerOptions {
    /// Configuration.
    pub config: Config,
    /// Bootstrap the dev org, repo, user and token.
    pub dev: bool,
    /// Clock (system clock by default).
    pub clock: Arc<dyn Clock>,
    /// Forge land inputs and VCS-path hooks; `None` = the built-in forge (when
    /// `forge.enabled`), or none at all.
    pub forge: Option<Arc<dyn ForgeInputs>>,
    /// The forge store to open instead of the configured database (tests use it to take
    /// the forge down).
    pub forge_store: Option<Arc<dyn bee_forge::store::ForgeStore>>,
    /// Registers more services after the built-in ones (CI, runners, and other areas built
    /// outside this crate). A service registered here replaces nothing: names must differ.
    pub extra: Option<ExtraServices>,
    /// Write `<data_dir>/credentials` in dev mode.
    pub write_credentials: bool,
    /// Named fault points (SPEC §14.4) around the metadata and blob stores: when set, every
    /// store call checks `meta.<method>` / `blob.<method>` on this injector. Conformance and
    /// fault tests use it to inject errors, delays and "committed, response lost" into a
    /// running server; production leaves it `None`.
    pub faults: Option<Arc<FaultInjector>>,
}

impl ServerOptions {
    /// Options for `config` with defaults.
    pub fn new(config: Config) -> ServerOptions {
        ServerOptions {
            config,
            dev: false,
            clock: Arc::new(SystemClock),
            forge: None,
            forge_store: None,
            extra: None,
            write_credentials: false,
            faults: None,
        }
    }
}

/// What dev mode created.
#[derive(Clone, Debug)]
pub struct DevBootstrap {
    /// The `dev` org.
    pub org: OrgId,
    /// `dev/playground`.
    pub repo: RepoId,
    /// The dev user's principal id.
    pub user: String,
    /// A root token for the dev user.
    pub token: String,
    /// The dev reviewer's principal id: a second human (user `reviewer`, a member of `dev`)
    /// so a change the dev user authored can be approved by someone else and landed.
    pub reviewer: String,
    /// A root token for the dev reviewer.
    pub reviewer_token: String,
}

/// A running server.
pub struct RunningServer {
    /// The bound address.
    pub addr: SocketAddr,
    /// `http://<addr>` (or the configured public URL).
    pub url: String,
    /// Shared state (tests inspect metrics, the store and coalescer leases through it).
    pub state: Arc<AppState>,
    /// Dev-mode bootstrap, when requested.
    pub dev: Option<DevBootstrap>,
    /// The built-in forge, when it runs.
    pub forge: Option<Arc<crate::forge::ForgeWiring>>,
    /// The Pages listener's address, when `forge.pages_listen` is set.
    pub pages_addr: Option<SocketAddr>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    background: Vec<tokio::task::JoinHandle<()>>,
}

impl RunningServer {
    /// Stops accepting connections and waits for the listener task.
    pub async fn shutdown(mut self) {
        self.stop().await;
    }

    /// Simulates a crash: the listener and every background and coalescer task stop at once,
    /// and coalescer leases stay held until they expire.
    pub async fn crash(mut self) {
        self.state.coalescers.abort();
        self.stop().await;
    }

    async fn stop(&mut self) {
        self.state.coalescers.shutdown();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        for b in self.background.drain(..) {
            b.abort();
        }
        if let Some(f) = &self.forge {
            f.stop().await;
        }
        if let Some(t) = self.task.take() {
            t.abort();
            let _ = t.await;
        }
    }

    /// Waits until the server task ends.
    pub async fn wait(mut self) {
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }

    /// Waits until the server task ends or `stop` completes. On `stop` the listener stops
    /// accepting and requests in flight get up to `drain` to finish, then everything stops as
    /// in [`RunningServer::shutdown`] (coalescers release their leases).
    pub async fn wait_or_stop(
        mut self,
        stop: impl std::future::Future<Output = ()>,
        drain: Duration,
    ) {
        let Some(mut task) = self.task.take() else {
            return;
        };
        tokio::select! {
            _ = &mut task => {}
            () = stop => {
                if let Some(tx) = self.shutdown.take() {
                    let _ = tx.send(());
                }
                // A finished handle must not be awaited again: keep it only when the drain
                // ran out, so `stop` aborts what is left.
                if tokio::time::timeout(drain, &mut task).await.is_err() {
                    self.task = Some(task);
                }
                self.stop().await;
            }
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.state.coalescers.shutdown();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        for b in self.background.drain(..) {
            b.abort();
        }
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(bee_proto::v1::ErrorReason::Internal, e.to_string())
}

async fn open_stores(
    cfg: &Config,
    clock: Arc<dyn Clock>,
    faults: Option<Arc<FaultInjector>>,
) -> ApiResult<(Arc<dyn MetaStore>, Arc<dyn BlobStore>, Arc<dyn Kv>)> {
    std::fs::create_dir_all(&cfg.server.data_dir).map_err(internal)?;
    let m = &cfg.metadata;
    let (meta, kv): (Arc<dyn MetaStore>, Arc<dyn Kv>) = match m.backend.as_str() {
        "postgres" => {
            let mut o = PgOptions::new(m.postgres_url.clone());
            o.replica_url =
                (!m.postgres_replica_url.is_empty()).then(|| m.postgres_replica_url.clone());
            o.schema = (!m.postgres_schema.is_empty()).then(|| m.postgres_schema.clone());
            o.max_connections = m.max_connections;
            o.query_timeout = Duration::from_millis(m.query_timeout_ms);
            o.read_wait = Duration::from_millis(m.read_wait_ms);
            let store = PgMetaStore::connect(o).await?;
            let kv = PgKv::connect(&m.postgres_url, o_schema(m), 4).await?;
            (with_meta_faults(store, &faults), Arc::new(kv))
        }
        _ => {
            let mut o = SqliteOptions::new(cfg.data_path(&m.sqlite_path, "meta.db"));
            o.query_timeout = Duration::from_millis(m.query_timeout_ms);
            o.read_wait = Duration::from_millis(m.read_wait_ms);
            let store = SqliteMetaStore::open(o).await?;
            let kv = SqliteKv::open(&cfg.data_path(&m.server_sqlite_path, "server.db"))?;
            (with_meta_faults(store, &faults), Arc::new(kv))
        }
    };
    let b = &cfg.blobs;
    let blobs: Arc<dyn BlobStore> = match b.backend.as_str() {
        "s3" => with_blob_faults(S3BlobStore::new(s3_config(cfg))?, &faults),
        _ => with_blob_faults(
            DiskBlobStore::open_with(
                cfg.data_path(&b.disk_root, "blobs"),
                DiskBlobConfig {
                    zstd_level: b.zstd_level,
                    ..DiskBlobConfig::default()
                },
                clock,
            )?,
            &faults,
        ),
    };
    Ok((meta, blobs, kv))
}

/// The metadata store as served: wrapped in [`FaultyMeta`] when a fault injector is set.
fn with_meta_faults<S: MetaStore + 'static>(
    store: S,
    faults: &Option<Arc<FaultInjector>>,
) -> Arc<dyn MetaStore> {
    match faults {
        Some(f) => Arc::new(FaultyMeta::new(store, f.clone())),
        None => Arc::new(store),
    }
}

/// The blob store as served: wrapped in [`FaultyBlob`] when a fault injector is set.
fn with_blob_faults<S: BlobStore + 'static>(
    store: S,
    faults: &Option<Arc<FaultInjector>>,
) -> Arc<dyn BlobStore> {
    match faults {
        Some(f) => Arc::new(FaultyBlob::new(store, f.clone())),
        None => Arc::new(store),
    }
}

fn o_schema(m: &crate::config::MetadataSection) -> Option<&str> {
    (!m.postgres_schema.is_empty()).then_some(m.postgres_schema.as_str())
}

fn s3_config(cfg: &Config) -> S3Config {
    let b = &cfg.blobs;
    let mut c = S3Config::new(
        b.s3_bucket.clone(),
        b.s3_region.clone(),
        S3Credentials::new(b.s3_access_key_id.clone(), b.s3_secret_access_key.clone()),
    );
    if !b.s3_endpoint.is_empty() {
        c = c.with_endpoint(b.s3_endpoint.clone());
    }
    c.prefix = b.s3_prefix.clone();
    c
}

pub(crate) fn url_key(cfg: &Config) -> ApiResult<Vec<u8>> {
    if !cfg.blobs.url_key_hex.is_empty() {
        return bee_core::hex::decode(&cfg.blobs.url_key_hex)
            .filter(|k| k.len() >= 32)
            .ok_or_else(|| internal("blobs.url_key_hex must be at least 64 hex digits"));
    }
    let path = cfg.server.data_dir.join("url.key");
    match std::fs::read(&path) {
        Ok(k) if k.len() >= 32 => Ok(k),
        _ => {
            let k = crate::util::random_bytes(32)?;
            std::fs::write(&path, &k).map_err(internal)?;
            Ok(k)
        }
    }
}

/// Creates a repo with its empty root changeset (SPEC §4.4) and, optionally, `main@1`.
pub async fn create_repo(
    state: &AppState,
    org: &OrgRow,
    repo: &RepoRow,
    with_main: bool,
) -> ApiResult<()> {
    state.meta.create_repo(repo.clone()).await?;
    let empty_tree = Tree::empty();
    let empty_meta = Meta::empty_root();
    state
        .writer
        .upload(
            org.id,
            vec![
                (empty_tree.id(), empty_tree.encode()),
                (empty_meta.id(), empty_meta.encode()),
            ],
            None,
            state.now_ms(),
        )
        .await?;
    let ctx = crate::state::RepoCtx {
        full_name: format!("{}/{}", org.name, repo.name),
        org: org.clone(),
        repo: repo.clone(),
    };
    let history = state.history(&ctx);
    let built = state
        .writer
        .build(
            org.id,
            repo.id,
            Vec::new(),
            empty_meta.id(),
            ChangeId::new([0; 16]),
            Vec::new(),
        )
        .await?;
    let root = history.store_built(&built, state.now_ms()).await?;
    debug_assert_eq!(root, Changeset::empty_root().id());
    if with_main {
        let main = RefName::new("main").map_err(ApiError::from)?;
        if state.meta.get_ref(repo.id, &main).await?.is_none() {
            state
                .writer
                .update_ref(RefUpdate::set(
                    repo.id,
                    main,
                    Expected::Absent,
                    Some(root),
                    "server",
                    state.now_ms(),
                ))
                .await?;
        }
    }
    Ok(())
}

async fn ensure_org(state: &AppState, name: &str) -> ApiResult<OrgRow> {
    if let Some(o) = state.meta.org_by_name(name).await? {
        return Ok(o);
    }
    let org = OrgRow {
        id: OrgId(crate::util::new_ulid(state.now_ms())?),
        name: name.to_string(),
        created_at_ms: state.now_ms(),
    };
    match state.meta.create_org(org.clone()).await? {
        PutOutcome::Inserted | PutOutcome::AlreadyPresent => Ok(org),
    }
}

/// The dev user's principal id, created (and kept in the server store) on first use, so a
/// data directory keeps one dev user across restarts.
pub async fn dev_user_id(kv: &dyn crate::kv::Kv, now_ms: u64) -> ApiResult<String> {
    match kv.get(crate::auth::NS_COUNTER, b"dev_user").await? {
        Some(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        None => {
            let id = crate::util::new_ulid(now_ms)?.to_text();
            kv.put(
                crate::auth::NS_COUNTER,
                b"dev_user",
                id.clone().into_bytes(),
            )
            .await?;
            Ok(id)
        }
    }
}

/// Dev mode: org `dev`, user `dev` (an org admin; [`start`] also lists it in `ops.operators`),
/// repo `dev/playground` with `main@1`, and a root token for the user (30 days).
pub async fn bootstrap_dev(state: &AppState) -> ApiResult<DevBootstrap> {
    let org = ensure_org(state, "dev").await?;
    let user_id = dev_user_id(state.kv.as_ref(), state.now_ms()).await?;
    let rec = PrincipalRecord {
        id: user_id.clone(),
        kind: "human".into(),
        display: "dev".into(),
        org: org.id.0.to_text(),
        owner: String::new(),
        runtime: String::new(),
        purpose: String::new(),
        external_ids: Vec::new(),
        groups: Vec::new(),
        deactivated: false,
    };
    state
        .kv
        .put(
            NS_PRINCIPAL,
            user_id.as_bytes(),
            serde_json::to_vec(&rec).unwrap_or_default(),
        )
        .await?;
    state
        .kv
        .put(
            NS_ORG_ADMIN,
            format!("{}/{}", org.id.0.to_text(), user_id).as_bytes(),
            Vec::new(),
        )
        .await?;
    let repo = match state.meta.repo_by_name(org.id, "playground").await? {
        Some(r) => r,
        None => {
            let r = RepoRow {
                id: RepoId(crate::util::new_ulid(state.now_ms())?),
                org: org.id,
                name: "playground".into(),
                created_at_ms: state.now_ms(),
                draft_retention_days: bee_store::DEFAULT_DRAFT_RETENTION_DAYS,
            };
            create_repo(state, &org, &r, true).await?;
            r
        }
    };
    let now = state.now_ms() / 1000;
    let exp = now + 30 * 24 * 3600;
    let token = state
        .ring
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .mint(
            &bee_auth::MintRequest {
                principal: rec.to_auth(),
                on_behalf_of: Vec::new(),
                scope: bee_auth::Scope::new().expires_at(exp),
            },
            now,
        )
        .map_err(ApiError::from)?;
    crate::services::auth::record_token(
        state.kv.as_ref(),
        &TokenRecord {
            token_id: token.id.0.clone(),
            principal: user_id.clone(),
            parent: String::new(),
            on_behalf_of: Vec::new(),
            created_ms: state.now_ms(),
            expires_at: exp,
            description: "dev bootstrap".into(),
            scope: None,
        },
    )
    .await?;
    // The second human: approves what the dev user authors (not an org admin).
    let reviewer_id = dev_principal_id(state, b"dev_reviewer").await?;
    let reviewer = PrincipalRecord {
        id: reviewer_id.clone(),
        display: "reviewer".into(),
        ..rec.clone()
    };
    state
        .kv
        .put(
            NS_PRINCIPAL,
            reviewer_id.as_bytes(),
            serde_json::to_vec(&reviewer).unwrap_or_default(),
        )
        .await?;
    let reviewer_token = state
        .ring
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .mint(
            &bee_auth::MintRequest {
                principal: reviewer.to_auth(),
                on_behalf_of: Vec::new(),
                scope: bee_auth::Scope::new().expires_at(exp),
            },
            now,
        )
        .map_err(ApiError::from)?;
    crate::services::auth::record_token(
        state.kv.as_ref(),
        &TokenRecord {
            token_id: reviewer_token.id.0.clone(),
            principal: reviewer_id.clone(),
            created_ms: state.now_ms(),
            expires_at: exp,
            description: "dev bootstrap (reviewer)".into(),
            ..TokenRecord::default()
        },
    )
    .await?;
    // The admin list changed: rebuild the org default on next use.
    state.policies.forget_org(org.id);
    Ok(DevBootstrap {
        org: org.id,
        repo: repo.id,
        user: user_id,
        token: token.text,
        reviewer: reviewer_id,
        reviewer_token: reviewer_token.text,
    })
}

/// A dev principal's id, created once and kept under `key`.
async fn dev_principal_id(state: &AppState, key: &[u8]) -> ApiResult<String> {
    if let Some(b) = state.kv.get(crate::auth::NS_COUNTER, key).await? {
        return Ok(String::from_utf8_lossy(&b).into_owned());
    }
    let id = crate::util::new_ulid(state.now_ms())?.to_text();
    state
        .kv
        .put(crate::auth::NS_COUNTER, key, id.clone().into_bytes())
        .await?;
    Ok(id)
}

async fn register_services(state: &AppState) -> ApiResult<()> {
    for s in &state.cfg.auth.services {
        let existing: Option<ServiceRecord> = state
            .kv
            .get(NS_SERVICE, s.name.as_bytes())
            .await?
            .and_then(|b| serde_json::from_slice(&b).ok());
        let principal = match existing {
            Some(r) => r.principal,
            None => crate::util::new_ulid(state.now_ms())?.to_text(),
        };
        let rec = PrincipalRecord {
            id: principal.clone(),
            kind: "service".into(),
            display: if s.display.is_empty() {
                s.name.clone()
            } else {
                s.display.clone()
            },
            org: s.org.clone(),
            owner: String::new(),
            runtime: String::new(),
            purpose: String::new(),
            external_ids: Vec::new(),
            groups: Vec::new(),
            deactivated: false,
        };
        state
            .kv
            .put(
                NS_PRINCIPAL,
                principal.as_bytes(),
                serde_json::to_vec(&rec).unwrap_or_default(),
            )
            .await?;
        state
            .kv
            .put(
                NS_SERVICE,
                s.name.as_bytes(),
                serde_json::to_vec(&ServiceRecord {
                    principal,
                    secret_sha256: s.secret_sha256.to_ascii_lowercase(),
                })
                .unwrap_or_default(),
            )
            .await?;
    }
    Ok(())
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    let meta_ok = tokio::time::timeout(
        Duration::from_secs(2),
        state.meta.org_by_name("-readyz-probe-"),
    )
    .await;
    let kv_ok =
        tokio::time::timeout(Duration::from_secs(2), state.kv.get("health", b"readyz")).await;
    // A forge that is configured but did not open leaves a process that serves the VCS and
    // answers every forge RPC "not served here" (see `start`, which does not retry it). It must
    // not be Ready, or a balancer would send forge traffic to it; `/healthz` stays 200.
    if state.cfg.forge.enabled && state.hub.get().is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "forge unavailable: it is enabled but did not open\n",
        )
            .into_response();
    }
    match (meta_ok, kv_ok) {
        (Ok(Ok(_)), Ok(Ok(_))) => (StatusCode::OK, "ready\n").into_response(),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "metadata store unavailable\n",
        )
            .into_response(),
    }
}

async fn metrics_handler(State(state): State<Arc<AppState>>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
        .into_response()
}

fn query_param<'a>(q: &'a str, name: &str) -> Option<&'a str> {
    q.split('&')
        .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
}

async fn blob(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((org, id)): axum::extract::Path<(String, String)>,
    req: Request,
) -> Response {
    let fail = |code: StatusCode, msg: &str| (code, msg.to_string()).into_response();
    let Some(issuer) = &state.local_urls else {
        return fail(StatusCode::NOT_FOUND, "signed URLs are not served here");
    };
    let q = req.uri().query().unwrap_or("");
    let (Some(exp), Some(sig)) = (query_param(q, "exp"), query_param(q, "sig")) else {
        return fail(StatusCode::UNAUTHORIZED, "missing exp or sig");
    };
    let (org_id, object) = match issuer.verify(&org, &id, exp, sig) {
        Ok(v) => v,
        Err(e) if e.reason == bee_core::Reason::InvalidArgument => {
            return fail(StatusCode::BAD_REQUEST, &e.message);
        }
        Err(e) => return fail(StatusCode::UNAUTHORIZED, &e.message),
    };
    // Repo access is decided again at redemption (`crate::access`): a repo deleted, made
    // private or out of the issuee's reach since the URL was issued refuses it.
    match state
        .redeem_url_grant(sig, query_param(q, "g"), query_param(q, "gs"))
        .await
    {
        Ok(()) => {}
        Err(e) if e.detail.reason == "UNAUTHENTICATED" => {
            return fail(StatusCode::UNAUTHORIZED, &e.detail.message);
        }
        Err(e) if e.detail.retryable => {
            return fail(StatusCode::SERVICE_UNAVAILABLE, &e.detail.message);
        }
        Err(_) => return fail(StatusCode::NOT_FOUND, "not found"),
    }
    // Never serve redacted or quarantined objects, even with a valid URL.
    if crate::redaction::is_redacted(&state, org_id, &object)
        .await
        .unwrap_or(true)
    {
        return fail(StatusCode::NOT_FOUND, "object is redacted");
    }
    match state.blobs.get(org_id, &object).await {
        Ok(Some(bytes)) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (
                    header::CACHE_CONTROL,
                    "private, max-age=60, immutable".to_string(),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => fail(StatusCode::NOT_FOUND, "not found"),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, &e.message),
    }
}

/// Starts a server: opens the stores, bootstraps dev mode when asked, binds the listener and
/// serves until shut down.
pub async fn start(opts: ServerOptions) -> ApiResult<RunningServer> {
    let mut cfg = opts.config;
    cfg.validate().map_err(|e| internal(e.0))?;
    logging::set_level(Level::parse(&cfg.logging.level));
    if let Some(warning) = crate::forge::local_runner::startup_warning(
        cfg.forge.local_runner.enabled.unwrap_or(false),
        opts.dev,
    ) {
        logging::log(Level::Warn, "forge.local_runner", warning, &[]);
    }
    let clock = opts.clock.clone();
    let (meta, blobs, kv) = open_stores(&cfg, clock.clone(), opts.faults.clone()).await?;
    // Reads keep being served from decoded rows while the metadata primary is down (SPEC §15.1).
    let meta: Arc<dyn MetaStore> = Arc::new(crate::outage_cache::OutageCache::new(
        meta,
        clock.clone(),
        cfg.metadata.stale_read_max_ms,
    ));
    if opts.dev {
        // The dev user runs this server: it is listed as an operator explicitly (org admins
        // are never operators), and the effective config shows it.
        let dev_user = dev_user_id(kv.as_ref(), clock.now_ms()).await?;
        if !cfg.ops.operators.contains(&dev_user) {
            cfg.ops.operators.push(dev_user);
        }
    }
    let listener = TcpListener::bind(&cfg.server.listen)
        .await
        .map_err(internal)?;
    let addr = listener.local_addr().map_err(internal)?;
    let tls = crate::tls::server_config(&cfg.tls)?;
    let url = if cfg.server.public_url.is_empty() {
        let scheme = if tls.is_some() { "https" } else { "http" };
        format!("{scheme}://{addr}")
    } else {
        cfg.server.public_url.trim_end_matches('/').to_string()
    };
    let (ring, keys) = crate::keys::load(&cfg, kv.as_ref(), clock.now_ms()).await?;
    let (url_issuer, local_urls): (
        Option<Arc<dyn SignedUrlIssuer>>,
        Option<Arc<LocalUrlIssuer>>,
    ) = if !cfg.blobs.signed_urls {
        (None, None)
    } else if cfg.blobs.backend == "s3" {
        let issuer = S3UrlIssuer::new(S3BlobStore::new(s3_config(&cfg))?)
            .with_max_ttl(Duration::from_secs(cfg.blobs.signed_url_ttl_secs));
        (Some(Arc::new(issuer)), None)
    } else {
        let local = Arc::new(
            LocalUrlIssuer::new(
                url.clone(),
                vec![url_key(&cfg)?],
                blobs.clone(),
                clock.clone(),
            )?
            .with_max_ttl(Duration::from_secs(cfg.blobs.signed_url_ttl_secs)),
        );
        (Some(local.clone() as Arc<dyn SignedUrlIssuer>), Some(local))
    };
    let metrics = Arc::new(Metrics::new());
    let audit: Arc<dyn AuditSink> = KvAudit::start(kv.clone(), metrics.clone());
    let hub = Arc::new(crate::forge::ForgeLink::new());
    let forge_inputs: Arc<dyn ForgeInputs> = match opts.forge {
        Some(f) => f,
        None => hub.clone(),
    };
    let state = AppState::new(StateParts {
        cfg: cfg.clone(),
        meta,
        blobs,
        kv,
        ring: Arc::new(RwLock::new(ring)),
        clock,
        url_issuer,
        local_urls,
        audit,
        metrics,
        forge: forge_inputs,
        hub: hub.clone(),
        keys,
    })?;
    *state.public_url.write().unwrap_or_else(|p| p.into_inner()) = url.clone();
    // Signing-key changes made by another replica are visible within access_cache_ms.
    let refresh_state = state.clone();
    let refresh_every = Duration::from_millis(cfg.forge.access_cache_ms.max(1));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(refresh_every).await;
            let _ = crate::keys::refresh(&refresh_state).await;
        }
    });
    if let Some(exporter) = &state.otel {
        exporter.spawn(Duration::from_millis(cfg.tracing.export_interval_ms));
    }
    state.revocations.refresh(state.kv.as_ref()).await?;
    register_services(&state).await?;
    let dev = if opts.dev {
        let d = bootstrap_dev(&state).await?;
        if opts.write_credentials {
            let path = cfg.server.data_dir.join("credentials");
            let text = format!(
                "endpoint = \"{url}\"\ntoken = \"{}\"\nreviewer = \"{}\"\nreviewer_token = \"{}\"\n",
                d.token, d.reviewer, d.reviewer_token
            );
            std::fs::write(&path, text).map_err(internal)?;
        }
        Some(d)
    } else {
        None
    };

    let forge = if cfg.forge.enabled {
        match open_forge(&state, opts.forge_store, dev.as_ref()).await {
            Ok(w) => Some(w),
            Err(e) => {
                // A separate failure domain: the VCS serves without the forge.
                logging::log(
                    Level::Error,
                    "forge",
                    "the forge did not open; serving the VCS without it",
                    &[("error", e.detail.message.clone().into())],
                );
                None
            }
        }
    } else {
        None
    };
    let mut registry = ServiceRegistry::new(state.clone()).with_core();
    if let Some(w) = &forge {
        w.register(&mut registry);
    }
    if let Some(extra) = opts.extra {
        extra(&mut registry);
    }
    let (grpc, gateway) = registry.into_routers();
    let mcp = bee_mcp::mcp_router(bee_mcp::McpConfig::new(bee_mcp::ClientConfig::new(
        url.clone(),
    )))
    .map_err(|e| internal(format!("mcp: {e:?}")))?;
    let git_gateway = crate::git_mount::gateway(state.clone())?;
    let _ = state.git_gateway.set(git_gateway.clone());
    let http = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_handler))
        .route("/blob/{org}/{id}", get(blob))
        .with_state(state.clone())
        .nest("/mcp", mcp)
        .merge(gateway)
        .merge(crate::forge::http::router(state.clone()))
        .merge(crate::forge::packages::registry_router(state.clone()))
        .merge(git_gateway.router());
    let http = match &forge {
        Some(w) => http.merge(crate::forge::http::compat_router(&state, w)?),
        None => http,
    };
    let trust_xff = cfg.server.trust_forwarded_for;
    let peer_tokens = Arc::new(crate::tls::PeerTokens::default());
    let peers = peer_tokens.clone();
    let dispatch = tower::service_fn(move |mut req: Request<Body>| {
        let grpc = grpc.clone();
        let http = http.clone();
        let peers = peers.clone();
        async move {
            // A connection authenticated by a mapped client certificate (mutual TLS) speaks
            // as its principal unless the request brings its own credential.
            if !req.headers().contains_key(header::AUTHORIZATION)
                && let Some(ci) = req
                    .extensions()
                    .get::<axum::extract::ConnectInfo<SocketAddr>>()
                && let Some(token) = peers.get(&ci.0)
                && let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {token}"))
            {
                req.headers_mut().insert(header::AUTHORIZATION, v);
            }
            client_address(&mut req, trust_xff);
            let is_grpc = req
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/grpc"));
            let resp = if is_grpc {
                grpc.oneshot(req).await
            } else {
                http.oneshot(req).await
            };
            Ok::<_, Infallible>(resp.unwrap_or_else(|e| match e {}))
        }
    });
    let app = Router::new().fallback_service(dispatch);
    let (tx, rx) = oneshot::channel::<()>();
    let task = match tls {
        None => tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
        }),
        Some(config) => {
            use axum::serve::ListenerExt;
            let tls_listener = crate::tls::listen(listener, config, state.clone(), peer_tokens)
                .map_err(internal)?
                .tap_io(|_| {});
            tokio::spawn(async move {
                let _ = axum::serve(
                    tls_listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
            })
        }
    };
    let mut background = crate::services::spawn_workers(&state, forge.as_ref());
    if let Some(ssh) = crate::git_ssh::spawn_from_env(&state, &git_gateway).await? {
        background.push(ssh);
    }
    let mut pages_addr = None;
    if forge.is_some() && !cfg.forge.pages_listen.is_empty() {
        let pl = TcpListener::bind(&cfg.forge.pages_listen)
            .await
            .map_err(internal)?;
        pages_addr = Some(pl.local_addr().map_err(internal)?);
        let app = crate::forge::packages::pages_router(state.clone());
        background.push(tokio::spawn(async move {
            let _ = axum::serve(pl, app).await;
        }));
    }
    {
        let s = state.clone();
        background.push(tokio::spawn(async move {
            let every = Duration::from_millis(s.cfg.auth.revocation_refresh_ms.max(100));
            loop {
                tokio::time::sleep(every).await;
                let _ = s.revocations.refresh(s.kv.as_ref()).await;
            }
        }));
    }
    {
        let s = state.clone();
        background.push(tokio::spawn(async move {
            let every = Duration::from_secs(s.cfg.redaction.purge_interval_secs.max(1));
            loop {
                tokio::time::sleep(every).await;
                if let Err(e) = crate::redaction::purge_due(&s).await {
                    logging::log(
                        Level::Warn,
                        "redaction",
                        "scheduled purge failed",
                        &[("error", e.detail.message.into())],
                    );
                }
            }
        }));
    }
    logging::log(
        Level::Info,
        "server",
        "listening",
        &[
            ("addr", addr.to_string().into()),
            ("cell", cfg.server.cell.clone().into()),
        ],
    );
    Ok(RunningServer {
        addr,
        url,
        state,
        dev,
        forge,
        pages_addr,
        shutdown: Some(tx),
        task: Some(task),
        background,
    })
}

/// Normalizes the client address: the TCP peer (or, with `trust_forwarded_for`, the first
/// `X-Forwarded-For` entry) is written to `X-Forwarded-For`, so a client cannot claim an
/// address that IP allow lists would admit.
fn client_address(req: &mut Request<Body>, trust_xff: bool) {
    let forwarded = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string());
    let ip = match (trust_xff, forwarded, peer) {
        (true, Some(f), _) => Some(f),
        (_, _, Some(p)) => Some(p),
        _ => None,
    };
    match ip.and_then(|ip| http::HeaderValue::from_str(&ip).ok()) {
        Some(v) => {
            req.headers_mut().insert("x-forwarded-for", v);
        }
        None => {
            req.headers_mut().remove("x-forwarded-for");
        }
    }
}

/// Opens the built-in forge, gives the dev org and repo their forge rows, and starts its
/// workers.
async fn open_forge(
    state: &Arc<AppState>,
    store: Option<Arc<dyn bee_forge::store::ForgeStore>>,
    dev: Option<&DevBootstrap>,
) -> ApiResult<Arc<crate::forge::ForgeWiring>> {
    let store = match store {
        Some(s) => s,
        None => crate::forge::open_store(state).await?,
    };
    let w = crate::forge::ForgeWiring::open(state, store).await?;
    if let Some(d) = dev {
        let repo = state
            .repo(Some(&bee_proto::v1::RepoRef {
                repo: Some(bee_proto::v1::repo_ref::Repo::Id(d.repo.0.to_text())),
            }))
            .await?;
        let owner = crate::forge::owner_principal(&d.user, "dev");
        w.directory
            .register(owner.clone(), vec!["dev".into()], Vec::new());
        w.adopt(&repo, &owner).await?;
        // The playground is created as `system`, which AutoFollow skips; the dev user follows
        // it here so a fresh `/feed` is not empty. Dev bootstrap only.
        let cx = bee_forge::Cx::system();
        let rec = w
            .forge
            .orgs
            .repo_any(&cx, &d.repo.0.to_text())
            .await
            .map_err(crate::forge::forge_to_api)?;
        w.forge
            .community
            .users
            .follow_repo_as(&cx, &d.user, &rec)
            .await
            .map_err(crate::forge::forge_to_api)?;
        let reviewer = crate::forge::owner_principal(&d.reviewer, "reviewer");
        w.directory
            .register(reviewer.clone(), vec!["reviewer".into()], Vec::new());
        w.add_member(&repo, &reviewer).await?;
    }
    state.hub.set(w.clone());
    w.spawn();
    Ok(w)
}

/// The data directory dev mode uses: `./.bee-dev`, or a fresh temporary directory.
pub fn dev_data_dir(temp: bool) -> std::io::Result<PathBuf> {
    if temp {
        let dir = std::env::temp_dir().join(format!("bee-dev-{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    } else {
        Ok(PathBuf::from(".bee-dev"))
    }
}
