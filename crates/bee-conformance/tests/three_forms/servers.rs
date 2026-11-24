//! In-process `bee-server --dev` instances on SQLite: one seed server runs the shared
//! scenario, then every group of fixtures gets three servers started on copies of the seed's
//! data directory (one per form), so all three begin from byte-identical state.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use bee_conformance::parity::fixture::{Commit, Step};
use bee_conformance::parity::normalize::{Normalizer, Outcome};
use bee_conformance::parity::template::{Vars, expand};
use bee_proto::methods::find_method;
use bee_server::config::Config;
use bee_server::server::{RunningServer, ServerOptions, start};
use serde_json::{Value, json};

use crate::api::{self, Client};

/// A running server and what the forms need to reach it.
pub struct Server {
    /// The server.
    pub srv: RunningServer,
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    /// The dev user's token.
    pub token: String,
    /// The dev user's principal id.
    pub user: String,
    /// The data directory (kept alive for the server's lifetime).
    pub dir: tempfile::TempDir,
    /// A private config directory for CLI processes (no saved credentials).
    pub cli_config: tempfile::TempDir,
}

/// The config every server uses: dev mode on a loopback port, fast forge workers.
pub fn config(dir: &Path) -> Config {
    let mut cfg = Config::dev(dir.to_path_buf(), Some("127.0.0.1:0".into()));
    cfg.redaction.purge_interval_secs = 3600;
    cfg.forge.relay_poll_ms = 50;
    cfg.forge.webhook_poll_ms = 50;
    cfg.forge.bridge_interval_ms = 50;
    cfg.forge.agent_runner = "fake".into();
    cfg.logging.level = "error".into();
    cfg
}

/// Starts a dev server on `dir` (a fresh one, or a copy of the seed).
pub async fn start_on(dir: tempfile::TempDir) -> Server {
    let mut opts = ServerOptions::new(config(dir.path()));
    opts.dev = true;
    let srv = start(opts).await.expect("server starts");
    let dev = srv.dev.clone().expect("dev bootstrap");
    Server {
        url: srv.url.clone(),
        token: dev.token,
        user: dev.user,
        srv,
        dir,
        cli_config: tempfile::tempdir().expect("tempdir"),
    }
}

impl Server {
    /// The built-in variables: `repo`, `org`, `user`, `server`, `token`.
    pub fn builtins(&self, vars: &mut Vars) {
        vars.set("repo", json!("dev/playground"));
        vars.set("org", json!("dev"));
        vars.set("user", json!(self.user));
        vars.set("userName", json!("dev"));
        vars.set("server", json!(self.url));
        vars.set("token", json!(self.token));
    }

    /// The normaliser for this server's responses.
    pub fn normalizer(&self) -> Normalizer {
        let host = self.url.trim_start_matches("http://").to_owned();
        Normalizer::new()
            .literal(&self.url, "<server>")
            .literal(&host, "<server-host>")
            .literal(&self.token, "<token>")
    }

    /// Stops the server.
    pub async fn stop(self) {
        self.srv.shutdown().await;
    }
}

/// Copies a directory tree.
pub fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// A copy of `seed` in a fresh temporary directory.
pub fn clone_dir(seed: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    copy_tree(seed, dir.path()).expect("copy seed");
    dir
}

/// Runs one step through the API on `server`, saving its result in `vars`.
pub async fn run_step(
    http: &Client,
    server: &Server,
    step: &Step,
    vars: &mut Vars,
) -> Result<(), String> {
    let saved = if let Some(commit) = &step.commit {
        run_commit(http, server, commit, vars).await?
    } else if let Some(up) = &step.upload {
        let text = |s: &str| -> Result<String, String> {
            Ok(expand(&json!(s), vars)?
                .as_str()
                .unwrap_or_default()
                .to_owned())
        };
        let body = match (&up.text, &up.base64) {
            (Some(t), _) => text(t)?.into_bytes(),
            (None, Some(b)) => base64::engine::general_purpose::STANDARD
                .decode(text(b)?)
                .map_err(|e| format!("upload: base64: {e}"))?,
            (None, None) => Vec::new(),
        };
        let mut req = http::Request::builder()
            .method(up.method.as_deref().unwrap_or("PUT"))
            .uri(text(&up.url)?);
        for (k, v) in &up.headers {
            req = req.header(k.as_str(), text(v)?);
        }
        if up.bearer {
            let token = match &step.token {
                Some(t) => text(t)?,
                None => server.token.clone(),
            };
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let req = req
            .body(http_body_util::Full::new(bytes::Bytes::from(body)))
            .map_err(|e| format!("upload: {e}"))?;
        let resp = http
            .request(req)
            .await
            .map_err(|e| format!("upload: {e}"))?;
        let status = resp.status().as_u16();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .map_err(|e| format!("upload: {e}"))?
            .to_bytes();
        let body = String::from_utf8_lossy(&bytes).into_owned();
        if !(200..300).contains(&status) {
            return Err(format!("upload: HTTP {status}: {body}"));
        }
        json!({ "status": status, "body": body })
    } else if let Some(seal) = &step.seal {
        let key = expand(&json!(seal.public_key), vars)?;
        let key = base64::engine::general_purpose::STANDARD
            .decode(key.as_str().unwrap_or_default())
            .map_err(|e| format!("seal: publicKey is not base64: {e}"))?;
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| "seal: publicKey is not 32 bytes".to_owned())?;
        let plaintext = expand(&json!(seal.plaintext), vars)?;
        let sealed =
            bee_forge::ci::sealed::seal(&key, plaintext.as_str().unwrap_or_default().as_bytes())
                .map_err(|e| format!("seal: {e:?}"))?;
        json!({ "sealed": b64(&sealed) })
    } else {
        let call = step.call.as_deref().unwrap_or_default();
        let path = format!("/bee.v1.{call}");
        let m = find_method(&path).ok_or(format!("no RPC {path}"))?;
        let request = expand(&step.request, vars)?;
        let token = match &step.token {
            Some(t) => expand(&json!(t), vars)?
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            None => server.token.clone(),
        };
        let r = api::call(http, &server.url, &token, m, &request, None).await;
        match (&r.outcome, step.expect.as_str()) {
            (Outcome::Ok(items), "ok") => {
                if items.len() == 1 {
                    items[0].clone()
                } else {
                    json!({ "items": items })
                }
            }
            (Outcome::Err(e), want) if e.reason == want => json!({"reason": e.reason}),
            (outcome, want) => {
                return Err(format!(
                    "setup {call} {}: want {want}, got {}",
                    api::describe(&request),
                    describe_outcome(outcome)
                ));
            }
        }
    };
    if let Some(name) = &step.save {
        vars.set(name, saved);
    }
    Ok(())
}

/// One-line description of an outcome.
pub fn describe_outcome(o: &Outcome) -> String {
    match o {
        Outcome::Ok(items) => format!("ok {}", api::describe(&Value::Array(items.clone()))),
        Outcome::Err(e) => format!("{} ({}): {}", e.reason, e.subcode, e.message),
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn id_json(text: &str) -> Result<Value, String> {
    let id = bee_core::ObjectId::parse(text).map_err(|e| format!("{text}: {e:?}"))?;
    Ok(json!({ "raw": b64(id.as_bytes()) }))
}

fn id_text(v: &Value) -> Result<String, String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(v["raw"].as_str().ok_or(format!("not an id: {v}"))?)
        .map_err(|e| e.to_string())?;
    Ok(bee_core::ObjectId::from_bytes(&raw)
        .map_err(|e| format!("{e:?}"))?
        .to_text())
}

async fn ok_call(
    http: &Client,
    server: &Server,
    path: &str,
    request: Value,
) -> Result<Value, String> {
    let m = find_method(path).ok_or(format!("no RPC {path}"))?;
    let r = api::call(http, &server.url, &server.token, m, &request, None).await;
    match r.outcome {
        Outcome::Ok(mut items) if items.len() == 1 => Ok(items.remove(0)),
        Outcome::Ok(items) => Ok(json!({ "items": items })),
        other => Err(format!(
            "{path} {}: {}",
            api::describe(&request),
            describe_outcome(&other)
        )),
    }
}

/// The built-in commit step: upload, create a changeset, then land or create a ref.
async fn run_commit(
    http: &Client,
    server: &Server,
    c: &Commit,
    vars: &Vars,
) -> Result<Value, String> {
    let repo_name = expand(&json!(c.repo), vars)?
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let repo = json!({ "fullName": repo_name });
    let parent_text = expand(&json!(c.parent), vars)?
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let parent = if bee_core::ObjectId::parse(&parent_text).is_ok() {
        id_json(&parent_text)?
    } else {
        let r = ok_call(
            http,
            server,
            "/bee.v1.ObjectService/ResolveRef",
            json!({ "repo": repo, "name": parent_text }),
        )
        .await?;
        r["ref"]["target"].clone()
    };
    let mut uploads = Vec::new();
    let mut delta = Vec::new();
    let mut blobs = serde_json::Map::new();
    for (i, (path, content)) in c.files.iter().enumerate() {
        let content = expand(&json!(content), vars)?
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let id = bee_object::blob_id(content.as_bytes()).to_text();
        blobs.insert(
            path.clone(),
            json!({ "id": id, "objectId": id_json(&id)?, "data": b64(content.as_bytes()) }),
        );
        let mut m = json!({ "id": id_json(&id)?, "offset": "0", "data": b64(content.as_bytes()), "last": true });
        if i == 0 {
            m["repo"] = repo.clone();
        }
        uploads.push(m);
        delta.push(json!({ "path": b64(path.as_bytes()), "put": {
            "mode": "MODE_FILE", "blob": id_json(&id)?, "size": content.len().to_string() } }));
    }
    for path in &c.delete {
        delta.push(json!({ "path": b64(path.as_bytes()), "delete": true }));
    }
    if !uploads.is_empty() {
        let m = find_method("/bee.v1.WriteService/UploadObjects").ok_or("no upload")?;
        let r = api::call(
            http,
            &server.url,
            &server.token,
            m,
            &Value::Array(uploads),
            None,
        )
        .await;
        if let Outcome::Err(e) = r.outcome {
            return Err(format!("upload: {} {}", e.reason, e.message));
        }
    }
    let mut cid = [0u8; 16];
    cid[..8].copy_from_slice(&c.change.to_be_bytes());
    cid[8] = 0xC0;
    let created = ok_call(
        http,
        server,
        "/bee.v1.WriteService/CreateChangeset",
        json!({
            "repo": repo,
            "parents": [parent],
            "delta": delta,
            "changeId": { "raw": b64(&cid) },
            "author": "conformance",
            "authorTime": { "name": "conformance", "time": "2026-10-01T00:00:00Z" },
            "message": c.message,
        }),
    )
    .await?;
    let cs = created["changeset"].clone();
    let text = id_text(&cs["id"])?;
    if let Some(r) = &c.land {
        ok_call(
            http,
            server,
            "/bee.v1.WriteService/Land",
            json!({ "repo": repo, "ref": r, "head": cs["id"] }),
        )
        .await?;
    }
    if let Some(r) = &c.create_ref {
        ok_call(
            http,
            server,
            "/bee.v1.RefService/CreateRef",
            json!({ "repo": repo, "name": r, "target": cs["id"] }),
        )
        .await?;
    }
    Ok(json!({
        "id": text,
        "objectId": cs["id"],
        "changeId": { "raw": b64(&cid) },
        "changeset": cs,
        "blobs": blobs,
    }))
}

/// Where fixtures live.
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}
