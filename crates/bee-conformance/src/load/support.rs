//! Helpers shared by the `load` and `vs_git` examples (included with `#[path]`, not part of the
//! library: they use the server, daemon and working-copy crates, which are dev-dependencies).

#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

use bee_proto::v1;
use serde_json::Value;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The value after `name` on the command line, parsed, or `default`.
pub fn arg(name: &str, default: usize) -> usize {
    arg_str(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The value after `name` on the command line.
pub fn arg_str(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// A comma-separated list of numbers after `name`, or `default`.
pub fn arg_list(name: &str, default: &[usize]) -> Vec<usize> {
    arg_str(name)
        .map(|v| v.split(',').filter_map(|n| n.trim().parse().ok()).collect())
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| default.to_vec())
}

/// CPU, cores, memory and OS of this machine.
pub fn machine() -> String {
    let sysctl = |k: &str| {
        std::process::Command::new("sysctl")
            .args(["-n", k])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let cpu = sysctl("machdep.cpu.brand_string").unwrap_or_else(|| std::env::consts::ARCH.into());
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    let mem = sysctl("hw.memsize")
        .and_then(|m| m.parse::<u64>().ok())
        .map_or_else(|| "unknown memory".into(), |b| format!("{} GiB", b >> 30));
    format!("{cpu}, {cores} cores, {mem}, {}", std::env::consts::OS)
}

/// The current UTC date and time, for a report header.
pub fn today() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M UTC"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// An error from the daemon crate's error detail.
pub fn detail(e: Box<v1::ErrorDetail>) -> BoxError {
    format!("{}: {}", e.reason, e.message).into()
}

/// Runs `f` and returns its output with the time it took.
pub async fn timed<T, F: std::future::Future<Output = T>>(f: F) -> (T, Duration) {
    let t0 = Instant::now();
    let out = f.await;
    (out, t0.elapsed())
}

/// `bee_wc::Transport` over the daemon crate's blocking Connect client.
pub struct WcTransport(pub bee_daemon::BlockingClient);

impl bee_wc::Transport for WcTransport {
    fn unary(&self, path: &str, request: &Value) -> Result<Value, Box<v1::ErrorDetail>> {
        self.0.unary(path, request)
    }
    fn server_stream(
        &self,
        path: &str,
        request: &Value,
    ) -> Result<Vec<Value>, Box<v1::ErrorDetail>> {
        self.0.server_stream(path, request)
    }
    fn server_streams(
        &self,
        path: &str,
        requests: &[Value],
    ) -> Vec<Result<Vec<Value>, Box<v1::ErrorDetail>>> {
        self.0.server_streams(path, requests)
    }
    fn list_tree(
        &self,
        request: &v1::ListTreeRequest,
    ) -> Option<Result<v1::ListTreeResponse, Box<v1::ErrorDetail>>> {
        Some(self.0.unary_proto("/bee.v1.QueryService/ListTree", request))
    }
    fn get_objects(
        &self,
        requests: &[v1::GetObjectsRequest],
    ) -> Option<Vec<Result<Vec<v1::ObjectChunk>, Box<v1::ErrorDetail>>>> {
        Some(
            self.0
                .server_streams_proto("/bee.v1.ObjectService/GetObjects", requests),
        )
    }
    fn client_stream(&self, path: &str, requests: &[Value]) -> Result<Value, Box<v1::ErrorDetail>> {
        self.0.client_stream(path, requests)
    }
}

/// A checkout of `sparse` of `dev/playground` into a fresh directory through `endpoint`;
/// returns the time.
pub fn checkout(
    endpoint: bee_daemon::Endpoint,
    token: &str,
    sparse: bee_wc::Sparse,
    dir: &Path,
) -> Result<Duration, BoxError> {
    let t = WcTransport(bee_daemon::BlockingClient::new(endpoint, Some(token)).map_err(detail)?);
    let opts = bee_wc::CreateOptions {
        endpoint: None,
        repo: "dev/playground".into(),
        target_ref: "main".into(),
        sparse,
        seed: None,
        adopt: false,
    };
    let t0 = Instant::now();
    bee_wc::create(dir, &opts, &t, &bee_wc::OpenOptions::default())?;
    Ok(t0.elapsed())
}

/// Runs the system `git` in `cwd` with `home` as `HOME` (its `.gitconfig` is the only config
/// read); returns stdout, or an error carrying stderr.
pub fn git_in(home: &Path, cwd: &Path, args: &[&str]) -> Result<String, BoxError> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs the system `git` in `home` (see [`git_in`]).
pub fn git(home: &Path, args: &[&str]) -> Result<(), BoxError> {
    git_in(home, home, args).map(|_| ())
}

/// Total size of the files under `p`.
pub fn dir_bytes(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            match e.metadata() {
                Ok(m) if m.is_dir() => total += dir_bytes(&path),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// Percent-encodes everything but RFC 3986 unreserved characters (a token in a URL).
pub fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
