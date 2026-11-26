//! Test support: a fake Connect HTTP/JSON server (axum, in-process), a runner for the real
//! `bee` binary, sample messages built from the descriptors, and JSON Schema validation
//! against the files in `schemas/v1/`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bee_cli::catalog::{Cardinality, Catalog, Kind, Scalar, catalog};
use bee_proto::connect::{ConnectError, EndStream};
use bee_proto::envelope::{self, FLAG_END_STREAM};
use bee_proto::errors::{error_detail, parse_reason, status_from_detail};
use bee_proto::methods::find_method;
use bee_proto::v1::ErrorDetail;
use serde_json::{Map, Value, json};

/// The repository root.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One request the fake server received.
#[derive(Clone, Debug)]
pub struct Received {
    /// RPC path.
    pub path: String,
    /// Content-Type header.
    pub content_type: String,
    /// Authorization header, if any.
    pub authorization: Option<String>,
    /// Connect-Timeout-Ms header, if any.
    pub timeout_ms: Option<String>,
    /// Request messages as JSON (one for unary).
    pub messages: Vec<Value>,
}

/// One item of a scripted stream.
#[derive(Clone, Debug)]
pub enum StreamStep {
    /// Send this message.
    Message(Value),
    /// Wait this long.
    Pause(Duration),
}

/// What the fake server answers for one RPC.
#[derive(Clone, Debug)]
pub enum Reply {
    /// The response message.
    Message(Value),
    /// A Connect error built from this detail.
    Error(ErrorDetail),
    /// A stream, then an end-stream frame carrying the optional error.
    Stream(Vec<StreamStep>, Option<ErrorDetail>),
    /// Wait, then answer the sample response (for timeouts).
    Delay(Duration),
}

#[derive(Default)]
struct ServerState {
    received: Mutex<Vec<Received>>,
    replies: Mutex<HashMap<String, Reply>>,
}

/// A running fake server.
pub struct FakeServer {
    /// `http://127.0.0.1:<port>` or `unix:<path>`.
    pub endpoint: String,
    state: Arc<ServerState>,
    _runtime: tokio::runtime::Runtime,
}

impl FakeServer {
    /// Starts a server on a free loopback port.
    pub fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let state = Arc::new(ServerState::default());
        let router = router(state.clone());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind(SocketAddr::from((
                [127, 0, 0, 1],
                0,
            ))))
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        runtime.spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        Self {
            endpoint: format!("http://{addr}"),
            state,
            _runtime: runtime,
        }
    }

    /// Starts a server on a unix socket at `path`.
    #[cfg(unix)]
    pub fn start_unix(path: &Path) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let state = Arc::new(ServerState::default());
        let router = router(state.clone());
        let listener = runtime
            .block_on(async { tokio::net::UnixListener::bind(path) })
            .expect("bind unix");
        runtime.spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        Self {
            endpoint: format!("unix:{}", path.display()),
            state,
            _runtime: runtime,
        }
    }

    /// Scripts the reply for one RPC path.
    pub fn reply(&self, path: &str, reply: Reply) {
        self.state
            .replies
            .lock()
            .expect("lock")
            .insert(path.to_owned(), reply);
    }

    /// Every request received so far.
    pub fn received(&self) -> Vec<Received> {
        self.state.received.lock().expect("lock").clone()
    }

    /// The last request received.
    pub fn last(&self) -> Received {
        self.received().pop().expect("a request")
    }
}

fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/{service}/{method}", post(handle))
        .with_state(state)
}

fn connect_error_response(detail: &ErrorDetail) -> Response {
    let error = ConnectError::from_status(&status_from_detail(detail));
    let status = parse_reason(&detail.reason)
        .and_then(bee_proto::errors::reason_info)
        .map_or(500, |info| info.http_status());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&error).expect("json")))
        .expect("response")
}

fn end_frame(error: Option<&ErrorDetail>) -> Bytes {
    let end = EndStream {
        error: error.map(|d| ConnectError::from_status(&status_from_detail(d))),
        metadata: Default::default(),
    };
    envelope::encode(FLAG_END_STREAM, &serde_json::to_vec(&end).expect("json"))
}

async fn handle(State(state): State<Arc<ServerState>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let (parts, body) = request.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let content_type = header("content-type").unwrap_or_default();
    let authorization = header("authorization");
    let timeout_ms = header("connect-timeout-ms");
    let Some(method) = find_method(&path) else {
        return connect_error_response(&error_detail(
            bee_proto::v1::ErrorReason::Unimplemented,
            format!("unknown {path}"),
        ));
    };
    let streaming = content_type == "application/connect+json";
    let body = axum::body::to_bytes(body, 64 << 20).await.expect("body");
    let raw_messages: Vec<Vec<u8>> = if streaming {
        envelope::decode_all(&body, 64 << 20)
            .expect("frames")
            .into_iter()
            .map(|f| f.data.to_vec())
            .collect()
    } else {
        vec![body.to_vec()]
    };
    let mut messages = Vec::new();
    for raw in &raw_messages {
        if let Err(e) = (method.request_json_to_proto)(raw) {
            return connect_error_response(&error_detail(
                bee_proto::v1::ErrorReason::InvalidArgument,
                format!("fake server: bad request JSON: {e}"),
            ));
        }
        messages.push(serde_json::from_slice(raw).unwrap_or(Value::Null));
    }
    state.received.lock().expect("lock").push(Received {
        path: path.clone(),
        content_type: content_type.clone(),
        authorization,
        timeout_ms,
        messages,
    });
    let reply = state
        .replies
        .lock()
        .expect("lock")
        .get(&path)
        .cloned()
        .unwrap_or_else(|| Reply::Message(sample_response(method.output_type)));
    match reply {
        Reply::Delay(wait) => {
            tokio::time::sleep(wait).await;
            json_response(streaming, sample_response(method.output_type))
        }
        Reply::Message(value) => json_response(streaming, value),
        Reply::Error(detail) if !streaming => connect_error_response(&detail),
        Reply::Error(detail) => stream_response(Vec::new(), Some(detail)),
        Reply::Stream(steps, error) => stream_response(steps, error),
    }
}

fn json_response(streaming: bool, value: Value) -> Response {
    if streaming {
        return stream_response(vec![StreamStep::Message(value)], None);
    }
    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&value).expect("json")))
        .expect("response")
}

fn stream_response(steps: Vec<StreamStep>, error: Option<ErrorDetail>) -> Response {
    let stream = futures_util::stream::unfold(
        (steps.into_iter(), Some(error)),
        |(mut steps, mut end)| async move {
            loop {
                match steps.next() {
                    Some(StreamStep::Pause(wait)) => tokio::time::sleep(wait).await,
                    Some(StreamStep::Message(value)) => {
                        let frame = envelope::encode(0, &serde_json::to_vec(&value).expect("json"));
                        return Some((Ok::<Bytes, std::convert::Infallible>(frame), (steps, end)));
                    }
                    None => {
                        let error = end.take()?;
                        return Some((Ok(end_frame(error.as_ref())), (steps, None)));
                    }
                }
            }
        },
    );
    Response::builder()
        .status(200)
        .header("content-type", "application/connect+json")
        .body(Body::from_stream(stream))
        .expect("response")
}

/// A valid object id in text form and as its proto3-JSON message.
pub fn object_id() -> (String, Value) {
    let id = bee_core::id::ObjectId::new(bee_core::id::ObjectKind::Changeset, [0xab; 32]);
    (id.to_text(), json!({"raw": STANDARD.encode(id.as_bytes())}))
}

fn sample_scalar(scalar: Scalar) -> Value {
    match scalar {
        Scalar::Double | Scalar::Float => json!(1.5),
        Scalar::Int32 | Scalar::Uint32 => json!(7),
        Scalar::Int64 | Scalar::Uint64 => json!("7"),
        Scalar::Bool => json!(true),
        Scalar::String => json!("sample"),
        Scalar::Bytes => json!(STANDARD.encode(b"sample")),
    }
}

fn sample_kind(catalog: &Catalog, kind: &Kind, depth: usize) -> Option<Value> {
    Some(match kind {
        Kind::Scalar(s) => sample_scalar(*s),
        Kind::Enum(name) => {
            let e = catalog.enum_type(name)?;
            let (value, _) = e
                .values
                .iter()
                .find(|(_, n)| *n != 0)
                .or_else(|| e.values.first())?;
            json!(value)
        }
        Kind::Message(name) => match name.as_str() {
            "google.protobuf.Timestamp" => json!("2026-01-02T03:04:05Z"),
            "google.protobuf.Duration" => json!("1.5s"),
            "google.protobuf.FieldMask" => json!("a.b"),
            "google.protobuf.Struct" | "google.protobuf.Empty" => json!({}),
            "google.protobuf.Value" => json!("v"),
            "google.protobuf.ListValue" => json!([]),
            "google.protobuf.Any" => return None,
            "google.protobuf.BoolValue" => json!(true),
            "google.protobuf.StringValue" => json!("s"),
            "google.protobuf.BytesValue" => json!("AA=="),
            "google.protobuf.Int32Value" | "google.protobuf.UInt32Value" => json!(3),
            "google.protobuf.Int64Value" | "google.protobuf.UInt64Value" => json!("3"),
            "google.protobuf.FloatValue" | "google.protobuf.DoubleValue" => json!(2.5),
            "bee.v1.ObjectId" => object_id().1,
            _ if depth >= 3 => return None,
            _ => sample_message(catalog, name, depth + 1),
        },
    })
}

fn sample_message(catalog: &Catalog, name: &str, depth: usize) -> Value {
    let Some(message) = catalog.message(name) else {
        return json!({});
    };
    let mut object = Map::new();
    let mut oneofs = Vec::new();
    for field in &message.fields {
        if let Some(oneof) = &field.oneof {
            if oneofs.contains(oneof) {
                continue;
            }
            oneofs.push(oneof.clone());
        }
        let value = match &field.cardinality {
            Cardinality::Single => sample_kind(catalog, &field.kind, depth),
            Cardinality::Repeated => {
                sample_kind(catalog, &field.kind, depth).map(|v| Value::Array(vec![v]))
            }
            Cardinality::Map(key, value) => sample_kind(catalog, value, depth).map(|v| {
                let key = match key {
                    Scalar::String => "k".to_owned(),
                    Scalar::Bool => "true".to_owned(),
                    _ => "1".to_owned(),
                };
                json!({ key: v })
            }),
        };
        if let Some(value) = value {
            object.insert(field.json_name.clone(), value);
        }
    }
    Value::Object(object)
}

/// A response message with every field set (to depth 3), in canonical proto3-JSON.
pub fn sample_response(type_name: &str) -> Value {
    let method = bee_proto::methods::METHODS
        .iter()
        .find(|m| m.output_type == type_name)
        .expect("an RPC returns this type");
    let raw = sample_message(catalog(), type_name, 0);
    let proto = (method.response_json_to_proto)(&serde_json::to_vec(&raw).expect("json"))
        .unwrap_or_else(|e| panic!("sample {type_name} is not valid: {e}\n{raw}"));
    let back = (method.response_proto_to_json)(&proto).expect("to json");
    serde_json::from_slice(&back).expect("json")
}

/// Result of one `bee` run.
#[derive(Debug)]
pub struct Run {
    /// Exit code.
    pub code: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

impl Run {
    /// Stdout as one JSON document.
    pub fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|e| {
            panic!(
                "stdout is not one JSON document ({e}):\n{}\nstderr: {}",
                self.stdout, self.stderr
            )
        })
    }

    /// Stdout as NDJSON.
    pub fn lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad line {l:?}: {e}")))
            .collect()
    }
}

/// A `bee` invocation with a private config directory and no inherited bee settings.
pub struct Bee {
    command: Command,
    stdin: Option<Vec<u8>>,
    /// The config directory used (holds `credentials`).
    pub config_dir: tempfile::TempDir,
    /// Holds the private link to the binary made by [`Bee::alone`].
    _bin_dir: Option<tempfile::TempDir>,
}

impl Bee {
    /// `bee <args>`.
    pub fn new(args: &[&str]) -> Self {
        let config_dir = tempfile::tempdir().expect("tempdir");
        Self::with_config(args, config_dir)
    }

    /// `bee <args>` run from a private link to the binary in a directory that holds nothing
    /// else. The CLI looks for helper programs such as `bee-github` beside itself, and a
    /// workspace build puts them next to the test binary, so a test about a missing helper has
    /// to run from a directory where it really is missing.
    pub fn alone(args: &[&str]) -> Self {
        let exe = Path::new(env!("CARGO_BIN_EXE_bee"));
        // Beside the real binary, so a hard link works (same filesystem). A link, not a copy: a
        // file this process has just written can still be open for writing in another test's
        // forked child, and running it then fails with "Text file busy".
        let bin_dir =
            tempfile::tempdir_in(exe.parent().expect("binary directory")).expect("tempdir");
        let program = bin_dir.path().join("bee");
        if std::fs::hard_link(exe, &program).is_err() {
            std::fs::copy(exe, &program).expect("copy bee");
        }
        let config_dir = tempfile::tempdir().expect("tempdir");
        let mut this = Self::with_program(&program, args, config_dir);
        this._bin_dir = Some(bin_dir);
        this
    }

    /// `bee <args>` using an existing config directory.
    pub fn with_config(args: &[&str], config_dir: tempfile::TempDir) -> Self {
        Self::with_program(Path::new(env!("CARGO_BIN_EXE_bee")), args, config_dir)
    }

    fn with_program(program: &Path, args: &[&str], config_dir: tempfile::TempDir) -> Self {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_remove("BEE_ENDPOINT")
            .env_remove("BEE_TOKEN")
            .env_remove("BEE_NO_DAEMON")
            // Never reach a daemon of the machine running the tests.
            .env("BEE_DAEMON_SOCKET", config_dir.path().join("none.sock"))
            .env("BEE_CONFIG_DIR", config_dir.path());
        Self {
            command,
            stdin: None,
            config_dir,
            _bin_dir: None,
        }
    }

    /// Sets an environment variable.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.command.env(key, value);
        self
    }

    /// Runs in this directory.
    pub fn current_dir(mut self, dir: &Path) -> Self {
        self.command.current_dir(dir);
        self
    }

    /// Feeds this to stdin.
    pub fn stdin(mut self, input: &str) -> Self {
        self.stdin = Some(input.as_bytes().to_vec());
        self
    }

    /// Runs and collects the result.
    pub fn run(mut self) -> (Run, tempfile::TempDir) {
        self.command
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = self.command.spawn().expect("spawn bee");
        if let Some(input) = self.stdin.take() {
            let mut pipe = child.stdin.take().expect("stdin");
            pipe.write_all(&input).expect("write stdin");
        }
        let output = child.wait_with_output().expect("wait");
        (
            Run {
                code: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
            self.config_dir,
        )
    }
}

/// Runs `bee <args>` with a fresh config directory.
pub fn bee(args: &[&str]) -> Run {
    Bee::new(args).run().0
}

/// Resolves `https://bee.dev/<path>` to the file at `<repo>/<path>`.
struct RepoRetriever;

impl jsonschema::Retrieve for RepoRetriever {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let path = uri
            .as_str()
            .strip_prefix("https://bee.dev/")
            .ok_or_else(|| format!("unexpected schema URI {}", uri.as_str()))?;
        let text = std::fs::read_to_string(repo_root().join(path))?;
        Ok(serde_json::from_str(&text)?)
    }
}

/// Validates `instance` against the schema file at `relative` (from the repository root).
pub fn validate(relative: &str, instance: &Value) {
    let text = std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|e| panic!("{relative}: {e}"));
    let schema: Value = serde_json::from_str(&text).expect("schema JSON");
    let validator = jsonschema::options()
        .with_retriever(RepoRetriever)
        .build(&schema)
        .unwrap_or_else(|e| panic!("{relative} does not compile: {e}"));
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{instance} does not match {relative}: {errors:#?}"
    );
}

/// Validates an error document and returns its `error` object.
pub fn validate_error(document: &Value) -> Value {
    validate("schemas/v1/error.schema.json", document);
    document["error"].clone()
}
