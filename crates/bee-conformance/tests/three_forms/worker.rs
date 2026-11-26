//! One process per fixture group. A server does not hand its threads back when it stops
//! (SQLite connection threads outlive `RunningServer::shutdown`), so 120+ servers in one
//! process run out of threads; each group therefore runs in a child process (this test binary
//! re-executed with [`WORKER`] set) that exits when the group is done.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bee_conformance::parity::report::{Row, Status};
use bee_conformance::parity::template::Vars;
use serde_json::{Value, json};

use crate::run::{self, MethodResult, Settings};

/// Set to the fully qualified service whose fixtures the child runs.
pub const WORKER: &str = "BEE_CONFORMANCE_WORKER";
/// The seed server's data directory.
const SEED: &str = "BEE_CONFORMANCE_SEED_DIR";
/// Directory with `globals.json` (scenario variables) and `tools.json` (MCP input schemas).
const STATE: &str = "BEE_CONFORMANCE_STATE_DIR";
/// Prefix of the result lines a child prints.
const RESULT: &str = "@@result ";

fn status(s: Status) -> &'static str {
    match s {
        Status::Pass => "pass",
        Status::Fail => "fail",
        Status::NotRun => "-",
    }
}

fn parse_status(s: &str) -> Status {
    match s {
        "pass" => Status::Pass,
        "fail" => Status::Fail,
        _ => Status::NotRun,
    }
}

fn to_json(r: &MethodResult) -> Value {
    json!({
        "service": r.row.service, "method": r.row.method, "expect": r.row.expect,
        "api": status(r.row.api), "cli": status(r.row.cli), "mcp": status(r.row.mcp),
        "failures": r.failures, "apiOutcome": r.api_outcome,
    })
}

fn from_json(v: &Value) -> MethodResult {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    MethodResult {
        row: Row {
            service: s("service"),
            method: s("method"),
            expect: s("expect"),
            api: parse_status(&s("api")),
            cli: parse_status(&s("cli")),
            mcp: parse_status(&s("mcp")),
        },
        failures: v["failures"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        api_outcome: s("apiOutcome"),
    }
}

/// Writes what children need into `dir`.
pub fn write_state(
    dir: &Path,
    globals: &Vars,
    tools: &BTreeMap<String, Value>,
) -> Result<(), String> {
    std::fs::write(
        dir.join("globals.json"),
        Value::Object(globals.values().clone()).to_string(),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(dir.join("tools.json"), json!(tools).to_string()).map_err(|e| e.to_string())
}

/// The child's main: runs one group and prints a result line per fixture.
pub fn worker_main(service: &str, settings: Settings) -> i32 {
    let state = PathBuf::from(std::env::var_os(STATE).unwrap_or_default());
    let seed = PathBuf::from(std::env::var_os(SEED).unwrap_or_default());
    let read = |name: &str| -> Value {
        std::fs::read_to_string(state.join(name))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null)
    };
    let mut globals = Vars::new();
    if let Value::Object(map) = read("globals.json") {
        for (k, v) in map {
            globals.set(&k, v);
        }
    }
    let tools: BTreeMap<String, Value> = match read("tools.json") {
        Value::Object(map) => map.into_iter().collect(),
        _ => BTreeMap::new(),
    };
    let file = match run::load() {
        Ok((_, files)) => match files.get(service) {
            Some((_, f)) => f.clone(),
            None => {
                eprintln!("no fixtures for {service}");
                return 1;
            }
        },
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let rt = crate::runtime();
    let results = rt.block_on(run::run_group(
        crate::api::client(),
        seed,
        globals,
        file,
        settings,
        Arc::new(tools),
    ));
    for r in &results {
        println!("{RESULT}{}", to_json(r));
    }
    0
}

/// Runs one group in a child process; relays its other output when `relay`.
pub async fn spawn_group(
    service: &str,
    seed: &Path,
    state: &Path,
    relay: bool,
) -> Result<Vec<MethodResult>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let output = tokio::process::Command::new(exe)
        .env(WORKER, service)
        .env(SEED, seed)
        .env(STATE, state)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("{service}: cannot start the worker: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        if let Some(json) = line.strip_prefix(RESULT) {
            let v: Value = serde_json::from_str(json).map_err(|e| format!("{service}: {e}"))?;
            results.push(from_json(&v));
        } else if relay && !line.starts_with('{') {
            println!("{line}");
        }
    }
    if !output.status.success() {
        return Err(format!(
            "{service}: worker failed ({}):\n{}\n{}",
            output.status,
            stdout
                .lines()
                .filter(|l| !l.starts_with(RESULT))
                .collect::<Vec<_>>()
                .join("\n"),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(results)
}
