//! Shared harness for the end-to-end tests: an in-process server (SQLite + disk, or Postgres
//! when `BEE_TEST_PG` is set) and small HTTP/JSON and gRPC helpers.

#![allow(dead_code, missing_docs, clippy::result_large_err)]

use std::time::Duration;

use base64::Engine as _;
use bee_mcp::{ClientConfig, RpcClient};
use bee_proto::v1::{self, ErrorDetail};
use bee_server::config::Config;
use bee_server::server::{RunningServer, ServerOptions, start};
use serde_json::{Value, json};

pub const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub struct TestServer {
    pub srv: RunningServer,
    pub token: String,
    pub dir: tempfile::TempDir,
    pub cfg: Config,
}

/// A config rooted at `dir`, on Postgres when `BEE_TEST_PG` is set.
pub fn test_config(dir: &std::path::Path) -> Config {
    let mut cfg = Config::dev(dir.to_path_buf(), Some("127.0.0.1:0".into()));
    cfg.redaction.purge_interval_secs = 3600;
    cfg.forge.relay_poll_ms = 50;
    cfg.forge.webhook_poll_ms = 50;
    cfg.forge.bridge_interval_ms = 50;
    if let Ok(url) = std::env::var("BEE_TEST_PG") {
        cfg.metadata.backend = "postgres".into();
        cfg.metadata.postgres_url = url;
        cfg.metadata.postgres_schema = format!("srv_{}", bee_server::util::random_hex(6));
    }
    cfg
}

pub async fn start_with(f: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = test_config(dir.path());
    f(&mut cfg);
    start_cfg(dir, cfg).await
}

/// The author time [`TestServer::commit`] stamps on every changeset.
pub const COMMIT_TIME_MS: u64 = 1_792_281_600_000;

/// A clock that starts an hour after [`COMMIT_TIME_MS`] and runs at real speed, so windows
/// measured back from "now" (trending, activity) keep covering the fixture's changesets.
#[derive(Debug)]
pub struct FixtureClock(std::time::Instant);

impl bee_store::Clock for FixtureClock {
    fn now_ms(&self) -> u64 {
        COMMIT_TIME_MS + 3_600_000 + self.0.elapsed().as_millis() as u64
    }
}

/// Like [`start_with`], on a [`FixtureClock`] instead of the system clock.
pub async fn start_at_fixture_time(f: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = test_config(dir.path());
    f(&mut cfg);
    start_opts(dir, cfg, |o| {
        o.clock = std::sync::Arc::new(FixtureClock(std::time::Instant::now()))
    })
    .await
}

pub async fn start_cfg(dir: tempfile::TempDir, cfg: Config) -> TestServer {
    start_opts(dir, cfg, |_| {}).await
}

async fn start_opts(
    dir: tempfile::TempDir,
    cfg: Config,
    f: impl FnOnce(&mut ServerOptions),
) -> TestServer {
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = true;
    f(&mut opts);
    let srv = start(opts).await.expect("server starts");
    let token = srv.dev.as_ref().expect("dev bootstrap").token.clone();
    TestServer {
        srv,
        token,
        dir,
        cfg,
    }
}

pub async fn start_default() -> TestServer {
    start_with(|_| {}).await
}

pub fn id_json(text: &str) -> Value {
    let id = bee_core::ObjectId::parse(text).expect("id");
    json!({ "raw": B64.encode(id.as_bytes()) })
}

pub fn id_text(v: &Value) -> String {
    let raw = B64.decode(v["raw"].as_str().expect("raw")).expect("base64");
    bee_core::ObjectId::from_bytes(&raw).expect("id").to_text()
}

pub fn change_id(n: u64) -> Value {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&n.to_be_bytes());
    b[8] = 0xA5;
    json!({ "raw": B64.encode(b) })
}

pub fn repo() -> Value {
    json!({ "fullName": "dev/playground" })
}

pub fn reason(e: &ErrorDetail) -> &str {
    &e.reason
}

impl TestServer {
    pub fn url(&self) -> &str {
        &self.srv.url
    }

    pub fn client(&self, token: &str) -> RpcClient {
        let mut c = ClientConfig::new(self.srv.url.clone());
        c.token = Some(token.to_string());
        c.request_timeout = Duration::from_secs(120);
        RpcClient::new(&c)
    }

    pub async fn call(
        &self,
        service: &str,
        method: &str,
        req: Value,
    ) -> Result<Value, ErrorDetail> {
        self.client(&self.token)
            .call_rpc_json(service, method, &req)
            .await
    }

    pub async fn call_as(
        &self,
        token: &str,
        service: &str,
        method: &str,
        req: Value,
    ) -> Result<Value, ErrorDetail> {
        self.client(token)
            .call_rpc_json(service, method, &req)
            .await
    }

    /// Uploads blobs over the HTTP/JSON client-streaming surface; returns their ids.
    pub async fn upload_as(&self, token: &str, blobs: &[&[u8]]) -> Vec<String> {
        let mut messages = Vec::new();
        let mut ids = Vec::new();
        for (i, b) in blobs.iter().enumerate() {
            let id = bee_object::blob_id(b).to_text();
            let mut m =
                json!({ "id": id_json(&id), "offset": "0", "data": B64.encode(b), "last": true });
            if i == 0 {
                m["repo"] = repo();
            }
            messages.push(m);
            ids.push(id);
        }
        self.call_as(
            token,
            "WriteService",
            "UploadObjects",
            json!({ "messages": messages }),
        )
        .await
        .expect("upload");
        ids
    }

    pub async fn upload(&self, blobs: &[&[u8]]) -> Vec<String> {
        self.upload_as(&self.token, blobs).await
    }

    /// Creates a changeset putting `files` on `parent`.
    pub async fn commit_as(
        &self,
        token: &str,
        parent: &str,
        files: &[(&str, &[u8])],
        cid: u64,
        message: &str,
    ) -> Result<Value, ErrorDetail> {
        let blobs: Vec<&[u8]> = files.iter().map(|(_, b)| *b).collect();
        let ids = self.upload_as(token, &blobs).await;
        let delta: Vec<Value> = files
            .iter()
            .zip(&ids)
            .map(|((p, b), id)| json!({ "path": B64.encode(p.as_bytes()), "put": { "mode": "MODE_FILE", "blob": id_json(id), "size": b.len().to_string() } }))
            .collect();
        let resp = self
            .call_as(
                token,
                "WriteService",
                "CreateChangeset",
                json!({
                    "repo": repo(),
                    "parents": [id_json(parent)],
                    "delta": delta,
                    "changeId": change_id(cid),
                    "author": "tester",
                    "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                    "message": message,
                }),
            )
            .await?;
        Ok(resp["changeset"].clone())
    }

    pub async fn commit(
        &self,
        parent: &str,
        files: &[(&str, &[u8])],
        cid: u64,
        message: &str,
    ) -> Value {
        self.commit_as(&self.token, parent, files, cid, message)
            .await
            .expect("commit")
    }

    pub async fn land_as(&self, token: &str, head: &str) -> Result<Value, ErrorDetail> {
        self.call_as(
            token,
            "WriteService",
            "Land",
            json!({ "repo": repo(), "ref": "main", "head": id_json(head) }),
        )
        .await
    }

    pub async fn land(&self, head: &str) -> Result<Value, ErrorDetail> {
        self.land_as(&self.token, head).await
    }

    pub async fn tip(&self, name: &str) -> (String, u64) {
        let r = self
            .call(
                "ObjectService",
                "ResolveRef",
                json!({ "repo": repo(), "name": name }),
            )
            .await
            .expect("resolve");
        (
            id_text(&r["ref"]["target"]),
            r["ref"]["version"].as_str().unwrap_or("0").parse().unwrap(),
        )
    }

    pub async fn read(&self, path: &str, at: &str) -> Result<Vec<u8>, ErrorDetail> {
        let r = self
            .call(
                "QueryService",
                "ReadPath",
                json!({ "repo": repo(), "at": { "expr": at }, "path": B64.encode(path) }),
            )
            .await?;
        let mut out = Vec::new();
        for item in r["items"].as_array().cloned().unwrap_or_default() {
            if let Some(d) = item["data"].as_str() {
                out.extend(B64.decode(d).unwrap());
            }
        }
        Ok(out)
    }

    pub async fn grpc_channel(&self) -> tonic::transport::Channel {
        tonic::transport::Endpoint::from_shared(self.srv.url.clone())
            .unwrap()
            .connect()
            .await
            .expect("grpc connect")
    }

    pub fn bearer<T>(&self, token: &str, msg: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(msg);
        r.metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        r
    }
}

/// A raw HTTP request (for `/mcp`, `/blob`, `/metrics`): status, headers, body.
pub async fn http(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (u16, http::HeaderMap, Vec<u8>) {
    use http_body_util::BodyExt;
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http::<http_body_util::Full<bytes::Bytes>>();
    let mut req = http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let req = req
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))
        .unwrap();
    let resp = client.request(req).await.expect("http request");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

/// Proto ids for gRPC calls.
pub fn pid(text: &str) -> v1::ObjectId {
    v1::ObjectId {
        raw: bee_core::ObjectId::parse(text).unwrap().as_bytes().to_vec(),
    }
}

pub fn prepo() -> Option<v1::RepoRef> {
    Some(v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::FullName("dev/playground".into())),
    })
}

pub fn pchange(n: u64) -> Option<v1::ChangeId> {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&n.to_be_bytes());
    b[8] = 0x5A;
    Some(v1::ChangeId { raw: b.to_vec() })
}

pub fn detail(s: &tonic::Status) -> ErrorDetail {
    bee_proto::errors::detail_from_status(s).expect("error detail")
}

impl TestServer {
    /// A principal of `kind` (`human`, `agent`, `service`) stored in the server's table, and
    /// a root token for it (30 minutes).
    pub async fn principal(&self, kind: &str, display: &str, owner: &str) -> (String, String) {
        let state = &self.srv.state;
        let id = bee_server::util::new_ulid(state.now_ms())
            .unwrap()
            .to_text();
        let rec = bee_server::auth::PrincipalRecord {
            id: id.clone(),
            kind: kind.into(),
            display: display.into(),
            org: self.srv.dev.as_ref().unwrap().org.0.to_text(),
            owner: owner.into(),
            runtime: if kind == "agent" {
                "claude-code".into()
            } else {
                String::new()
            },
            purpose: String::new(),
            external_ids: Vec::new(),
            groups: Vec::new(),
            deactivated: false,
        };
        state
            .kv
            .put(
                bee_server::auth::NS_PRINCIPAL,
                id.as_bytes(),
                serde_json::to_vec(&rec).unwrap(),
            )
            .await
            .unwrap();
        let token = self.token_for(&rec.to_auth(), &[]);
        (id, token)
    }

    /// A token for `principal` acting on behalf of `chain` (30 minutes).
    pub fn token_for(&self, principal: &bee_auth::Principal, chain: &[&str]) -> String {
        let state = &self.srv.state;
        let now = state.now_ms() / 1000;
        state
            .ring
            .read()
            .unwrap()
            .mint(
                &bee_auth::MintRequest {
                    principal: principal.clone(),
                    on_behalf_of: chain
                        .iter()
                        .map(|c| bee_auth::PrincipalRef::new(*c, bee_policy::PrincipalKind::Human))
                        .collect(),
                    scope: bee_auth::Scope::new().expires_at(now + 1800),
                },
                now,
            )
            .unwrap()
            .text
    }

    /// The dev user's id.
    pub fn dev_user(&self) -> String {
        self.srv.dev.as_ref().unwrap().user.clone()
    }

    /// Lands `.beehive/policy.toml` with the dev user as admin and `land` as the extra
    /// TOML (land rules), returning the new tip.
    pub async fn land_policy(&self, land: &str, cid: u64) -> String {
        let user = self.dev_user();
        let (tip, _) = self.tip("main").await;
        let policy = format!(
            "version = 1\n[members]\nadmin = [\"{user}\"]\nwrite = [\"@humans\", \"@agents\", \"@services\"]\nread = [\"@everyone\"]\n\
             [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n{land}"
        );
        let p = id_text(
            &self
                .commit(
                    &tip,
                    &[(".beehive/policy.toml", policy.as_bytes())],
                    cid,
                    "policy",
                )
                .await["id"],
        );
        self.land(&p).await.expect("policy lands");
        self.tip("main").await.0
    }
}

/// Polls `f` every 50 ms until it returns `Some`, for up to `secs` seconds.
pub async fn eventually<T, F, Fut>(secs: u64, what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
