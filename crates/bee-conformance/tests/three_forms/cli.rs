//! The CLI form: `bee --json <noun> <verb>` as a separate process. Top-level scalar fields
//! (and a `RepoRef` given by name) become flags; everything else goes in `--json-input`.
//! Every printed document is validated against `schemas/v1/` and the exit code against the
//! SPEC mapping.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bee_cli::catalog::{Cardinality, Kind, Scalar, catalog};
use bee_conformance::parity::contract::cli_exit_code;
use bee_conformance::parity::naming::{
    cli_flag, cli_noun, cli_schema_file, cli_schema_id, cli_verb,
};
use bee_conformance::parity::normalize::{ErrorFacts, Outcome};
use bee_proto::methods::{MethodDescriptor, MethodKind};
use serde_json::{Map, Value};

/// The repository root.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Environment variable that makes this test binary act as the `bee` CLI.
pub const AS_CLI: &str = "BEE_CONFORMANCE_AS_CLI";

/// The program to run as `bee`: `BEE_CONFORMANCE_BEE_BIN` (a built `bee` binary),
/// else this test binary re-executed with [`AS_CLI`] set, whose `main` is the same one line
/// as `bee`'s (`bee_cli::run(std::env::args_os())`).
pub fn program() -> (PathBuf, bool) {
    match std::env::var_os("BEE_CONFORMANCE_BEE_BIN") {
        Some(p) if !p.is_empty() => (PathBuf::from(p), false),
        _ => (std::env::current_exe().expect("current exe"), true),
    }
}

/// What a CLI call returned, with the contract problems seen.
pub struct CliResult {
    /// The outcome.
    pub outcome: Outcome,
    /// Envelope, schema and exit-code violations.
    pub problems: Vec<String>,
    /// The command line, for failure messages.
    pub command: String,
}

/// Splits a request into flags and the JSON input.
pub fn arguments(m: &MethodDescriptor, request: &Value) -> Vec<String> {
    let mut args = Vec::new();
    if matches!(
        m.kind,
        MethodKind::ClientStreaming | MethodKind::BidiStreaming
    ) {
        args.push("--json-input".to_owned());
        args.push(request.to_string());
        return args;
    }
    let Some(message) = catalog().message(m.input_type) else {
        args.push("--json-input".into());
        args.push(request.to_string());
        return args;
    };
    let mut rest = Map::new();
    if let Value::Object(fields) = request {
        for (key, value) in fields {
            let Some(field) = message.field(key) else {
                rest.insert(key.clone(), value.clone());
                continue;
            };
            let flag = cli_flag(&field.name);
            let text = match (&field.cardinality, &field.kind, value) {
                (_, _, _) if field.name == "follow" => None,
                (Cardinality::Single, Kind::Scalar(Scalar::Bytes), _) => None,
                (Cardinality::Single, Kind::Scalar(_) | Kind::Enum(_), Value::String(s)) => {
                    Some(s.clone())
                }
                (Cardinality::Single, Kind::Scalar(_), Value::Number(n)) => Some(n.to_string()),
                (Cardinality::Single, Kind::Scalar(_), Value::Bool(b)) => Some(b.to_string()),
                (Cardinality::Single, Kind::Message(t), Value::Object(o))
                    if t == "bee.v1.RepoRef" && o.len() == 1 =>
                {
                    o.get("fullName").and_then(Value::as_str).map(str::to_owned)
                }
                _ => None,
            };
            match text {
                Some(t) => args.push(format!("--{flag}={t}")),
                None => {
                    rest.insert(key.clone(), value.clone());
                }
            }
        }
    }
    if !rest.is_empty() {
        args.push("--json-input".into());
        args.push(Value::Object(rest).to_string());
    }
    args
}

/// Runs the command for `m`.
pub async fn call(
    url: &str,
    token: &str,
    config_dir: &Path,
    m: &MethodDescriptor,
    request: &Value,
) -> CliResult {
    let (program, as_cli) = program();
    let noun = cli_noun(m.service);
    let verb = cli_verb(m.method);
    let mut args: Vec<String> = vec![
        "--json".into(),
        "--endpoint".into(),
        url.into(),
        "--token".into(),
        token.into(),
        "--timeout".into(),
        "120s".into(),
        noun.clone(),
        verb.clone(),
    ];
    args.extend(arguments(m, request));
    let command = format!("bee --json {}", args[7..].join(" "));
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .env_remove("BEE_ENDPOINT")
        .env_remove("BEE_TOKEN")
        .env("BEE_CONFIG_DIR", config_dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if as_cli {
        cmd.env(AS_CLI, "1");
    }
    let output = match tokio::time::timeout(Duration::from_secs(180), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return failure(command, format!("cannot run the CLI: {e}")),
        Err(_) => return failure(command, "the CLI did not finish in 180 s".into()),
    };
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let mut problems = Vec::new();
    let docs: Vec<Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str::<Value>(l) {
            Ok(v) => Some(v),
            Err(e) => {
                problems.push(format!("stdout line is not JSON ({e}): {l}"));
                None
            }
        })
        .collect();
    let schema_id = cli_schema_id(m.service, m.method);
    let error_doc = docs.iter().find(|d| d["schema"] == "bee.v1.error");
    let outcome = if let Some(doc) = error_doc {
        validate("schemas/v1/error.schema.json", doc, &mut problems);
        let e = &doc["error"];
        let reason = e["reason"].as_str().unwrap_or_default().to_owned();
        let want = cli_exit_code(Some(&reason));
        if code != want {
            problems.push(format!("exit code {code} for {reason}, SPEC says {want}"));
        }
        Outcome::Err(ErrorFacts {
            reason,
            retryable: e["retryable"].as_bool().unwrap_or(false),
            subcode: e["payload"]["subcode"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            message: e["message"].as_str().unwrap_or_default().to_owned(),
        })
    } else {
        if code != 0 {
            problems.push(format!(
                "exit code {code} without an error document; stderr: {}",
                stderr.trim()
            ));
        }
        let file = cli_schema_file(m.service, m.method);
        let mut items = Vec::new();
        for doc in &docs {
            if doc["schema"] != schema_id.as_str() {
                problems.push(format!(
                    "document schema {} is not {schema_id}",
                    doc["schema"]
                ));
            }
            validate(&file, doc, &mut problems);
            items.push(doc["data"].clone());
        }
        if matches!(m.kind, MethodKind::Unary | MethodKind::ClientStreaming) && docs.len() != 1 {
            problems.push(format!("{} documents for a unary call, want 1", docs.len()));
        }
        Outcome::Ok(items)
    };
    CliResult {
        outcome,
        problems,
        command,
    }
}

fn failure(command: String, message: String) -> CliResult {
    CliResult {
        outcome: Outcome::Err(ErrorFacts {
            reason: "TRANSPORT".into(),
            retryable: false,
            subcode: String::new(),
            message: message.clone(),
        }),
        problems: vec![message],
        command,
    }
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

type Validators = Mutex<HashMap<String, Arc<Result<jsonschema::Validator, String>>>>;

fn validator(relative: &str) -> Arc<Result<jsonschema::Validator, String>> {
    static CACHE: OnceLock<Validators> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().expect("cache").get(relative) {
        return v.clone();
    }
    let built = std::fs::read_to_string(repo_root().join(relative))
        .map_err(|e| format!("{relative}: {e}"))
        .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| format!("{relative}: {e}")))
        .and_then(|schema| {
            jsonschema::options()
                .with_retriever(RepoRetriever)
                .build(&schema)
                .map_err(|e| format!("{relative} does not compile: {e}"))
        });
    let built = Arc::new(built);
    cache
        .lock()
        .expect("cache")
        .insert(relative.to_owned(), built.clone());
    built
}

/// Validates `instance` against the schema file, recording violations.
pub fn validate(relative: &str, instance: &Value, problems: &mut Vec<String>) {
    match validator(relative).as_ref() {
        Ok(v) => {
            for e in v.iter_errors(instance).take(3) {
                problems.push(format!("{relative}: {e} at {}", e.instance_path()));
            }
        }
        Err(e) => problems.push(e.clone()),
    }
}
