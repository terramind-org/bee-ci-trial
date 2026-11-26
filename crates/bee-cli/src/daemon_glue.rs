//! Glue between the CLI and the per-machine node daemon (`bee-daemon`, SPEC §10.2,
//! `docs/spec/cache-tiers.md`).
//!
//! The CLI reaches the daemon over its unix socket: `BEE_DAEMON_SOCKET`, else the per-user
//! default of [`bee_daemon::default_socket_path`]. Everything here degrades to the direct
//! path: `--no-daemon` (or `BEE_NO_DAEMON=1`), a missing socket, a socket nobody answers on,
//! or a daemon that was started for a different server all mean "no daemon", never an error.
//!
//! Two things use the link:
//!
//! - object reads of a working copy (`ObjectService/GetObjects`) go to the daemon, which
//!   coalesces and caches them ([`crate::wc::CliTransport`] routes them); everything else goes
//!   to the server;
//! - `bee status` asks the daemon's watcher which paths changed ([`watched_status`]) and
//!   re-examines only those; whenever the daemon cannot vouch for the whole interval it falls
//!   back to the stat-cache walk.
//!
//! `BEE_DAEMON_TRACE=1` prints one line per decision to stderr (for diagnosis and tests).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bee_core::{Mode, RepoPath};
use bee_daemon::{BlockingClient, Endpoint};
use bee_proto::v1::ErrorDetail;
use bee_wc::manifest::{self, Change, Manifest};
use bee_wc::scan::{self, StatCache};
use bee_wc::{Sparse, StatusReport, WorkingCopy, fsutil, report};
use serde_json::{Value, json};

use crate::error::CliError;

/// Environment variable naming the daemon socket.
pub const ENV_SOCKET: &str = "BEE_DAEMON_SOCKET";
/// Environment variable that disables the daemon when set to anything but `0` or empty.
pub const ENV_NO_DAEMON: &str = "BEE_NO_DAEMON";
/// Environment variable that makes the CLI explain its daemon decisions on stderr.
pub const ENV_TRACE: &str = "BEE_DAEMON_TRACE";

/// The sparse prefix of a virtual checkout before anything is touched: it names nothing.
pub const VIRTUAL_PREFIX: &str = ".bee-virtual";

/// File under `.bee/` that exists while the last full walk skipped names that are not valid
/// repo paths (the watcher journal cannot list those, so status keeps walking).
const SKIPPED_MARKER: &str = "daemon-skipped";

/// True unless the daemon is switched off by `--no-daemon` or `BEE_NO_DAEMON`.
pub fn enabled(no_daemon_flag: bool) -> bool {
    if no_daemon_flag {
        return false;
    }
    !matches!(std::env::var(ENV_NO_DAEMON), Ok(v) if !v.trim().is_empty() && v.trim() != "0")
}

/// Prints a decision line when `BEE_DAEMON_TRACE` is set.
pub fn trace(message: &str) {
    if matches!(std::env::var(ENV_TRACE), Ok(v) if !v.trim().is_empty() && v.trim() != "0") {
        eprintln!("bee: daemon: {message}");
    }
}

/// The socket to use: `explicit`, else `BEE_DAEMON_SOCKET`, else the per-user default.
pub fn socket_path(explicit: Option<&str>) -> PathBuf {
    match explicit
        .map(str::to_owned)
        .or_else(crate::client_config::daemon_socket)
    {
        Some(p) => PathBuf::from(p.strip_prefix("unix:").unwrap_or(&p)),
        None => bee_daemon::default_socket_path(),
    }
}

/// Sidecar file next to the socket naming the server a `bee daemon start` daemon fronts.
pub fn upstream_file(socket: &Path) -> PathBuf {
    sidecar(socket, "upstream")
}

/// Sidecar file next to the socket holding the pid of a `bee daemon start` daemon.
pub fn pid_file(socket: &Path) -> PathBuf {
    sidecar(socket, "pid")
}

/// Log file of a `bee daemon start` daemon.
pub fn log_file(socket: &Path) -> PathBuf {
    sidecar(socket, "log")
}

fn sidecar(socket: &Path, ext: &str) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

fn same_endpoint(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/')
}

/// A live connection to the node daemon.
pub struct Link {
    /// The socket it answers on.
    pub socket: PathBuf,
    client: BlockingClient,
}

impl Link {
    /// Connects to the daemon on `socket` if one answers (`Stats` is the liveness probe) and,
    /// when it records which server it fronts, that server is `endpoint`.
    pub fn connect(socket: &Path, token: Option<&str>, endpoint: Option<&str>) -> Option<Link> {
        if !is_socket(socket) {
            trace(&format!("no daemon socket at {}", socket.display()));
            return None;
        }
        if let (Some(endpoint), Ok(recorded)) =
            (endpoint, std::fs::read_to_string(upstream_file(socket)))
            && !same_endpoint(endpoint, &recorded)
        {
            trace(&format!(
                "the daemon on {} fronts {}, not {endpoint}; going direct",
                socket.display(),
                recorded.trim()
            ));
            return None;
        }
        let client = BlockingClient::new(Endpoint::Unix(socket.to_path_buf()), token).ok()?;
        match client.local("Stats", &json!({})) {
            Ok(_) => Some(Link {
                socket: socket.to_path_buf(),
                client,
            }),
            Err(e) => {
                trace(&format!(
                    "nothing answers on {} ({}); going direct",
                    socket.display(),
                    e.message
                ));
                None
            }
        }
    }

    /// A daemon-local operation (`/daemon/v1/<op>`).
    pub fn local(&self, op: &str, request: &Value) -> Result<Value, Box<ErrorDetail>> {
        self.client.local(op, request)
    }

    /// A unary call, the shape of `bee_wc::Transport::unary`.
    pub fn unary(&self, path: &str, request: &Value) -> Result<Value, Box<ErrorDetail>> {
        self.client.unary(path, request)
    }

    /// A server-streaming call, the shape of `bee_wc::Transport::server_stream`.
    pub fn server_stream(
        &self,
        path: &str,
        request: &Value,
    ) -> Result<Vec<Value>, Box<ErrorDetail>> {
        self.client.server_stream(path, request)
    }

    /// Starts the watcher on a working copy root; failures are not errors (status then walks).
    pub fn watch(&self, root: &Path) {
        let text = root.display().to_string();
        match self.local("Watch", &json!({ "root": text })) {
            Ok(_) => trace(&format!("watching {text}")),
            Err(e) => trace(&format!("cannot watch {text}: {}", e.message)),
        }
    }
}

#[cfg(unix)]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

#[cfg(not(unix))]
fn is_socket(_path: &Path) -> bool {
    false
}

/// `bee status` with a daemon linked: from the watcher's journal when it can vouch
/// ([`watched_status`]), else by walking the tree ([`walked_status`]).
pub fn status(link: &Link, wc: &WorkingCopy) -> Result<StatusReport, CliError> {
    match watched_status(link, wc) {
        Ok(report) => Ok(report),
        Err(why) => {
            trace(&format!("status walks the tree: {why}"));
            walked_status(wc)
        }
    }
}

/// The normal full walk (stat cache, re-hashing what changed), written with
/// `bee_wc::scan` so the refreshed stat cache always carries the walk's clock: the next
/// status can then start from the watcher even when the walk hashed nothing. Also records
/// whether invalid names were skipped, so a later watcher status does not pretend it knows
/// about them.
pub fn walked_status(wc: &WorkingCopy) -> Result<StatusReport, CliError> {
    let bee = wc.root().join(".bee");
    let cache = load_cache(&bee);
    let state = wc.state();
    let clock = scan::touch_clock(&bee).map_err(crate::wc::cli_error)?;
    let result = scan::scan(
        wc.root(),
        clock,
        &state.sparse,
        wc.manifest(),
        &state.hidden_set(),
        &cache,
    )
    .map_err(crate::wc::cli_error)?;
    fsutil::atomic_write(&bee.join("statcache"), &result.cache.encode())
        .map_err(crate::wc::cli_error)?;
    let marker = bee.join(SKIPPED_MARKER);
    if result.skipped.is_empty() {
        let _ = std::fs::remove_file(marker);
    } else {
        let _ = fsutil::atomic_write(&marker, b"1");
    }
    let changes = manifest::diff(wc.manifest(), &result.actual);
    Ok(report_of(wc, &changes, result.skipped))
}

fn load_cache(bee: &Path) -> StatCache {
    std::fs::read(bee.join("statcache"))
        .ok()
        .and_then(|b| StatCache::decode(&b).ok())
        .unwrap_or_default()
}

/// The status document for these uncommitted changes.
fn report_of(wc: &WorkingCopy, changes: &[Change], skipped: Vec<String>) -> StatusReport {
    let state = wc.state();
    let rebase = state.rebase.as_ref();
    StatusReport {
        root: wc.root().display().to_string(),
        repo: wc.config().repo.clone(),
        target_ref: wc.config().target_ref.clone(),
        head: state.head.to_text(),
        onto: state.onto.to_text(),
        onto_version: state.onto_version,
        stack: report::stack_views(state),
        clean: changes.is_empty() && rebase.is_none(),
        changes: report::file_changes(changes),
        conflicts: report::conflict_views(rebase),
        rebase: report::rebase_view(rebase),
        sparse: state.sparse.display(),
        hidden: report::hidden_views(&state.hidden),
        skipped,
        recovered: wc.recovered().map(str::to_owned),
    }
}

/// Status computed from the daemon's watcher journal: ask which paths changed since the stat
/// cache's clock, rescan only those with `bee_wc::scan`, merge with the stat cache, save the
/// refreshed cache and diff against the base manifest.
///
/// This is sound because any change to a file after the old clock produces an event at or
/// after it, so every entry that is not rescanned is unchanged since it was hashed. `Err`
/// carries why the journal could not vouch (the daemon said `fresh`, was unreachable, a name
/// is not a valid repo path, ...); the caller then runs the normal walk.
pub fn watched_status(link: &Link, wc: &WorkingCopy) -> Result<StatusReport, String> {
    let root = wc.root();
    let bee = root.join(".bee");
    if bee.join(SKIPPED_MARKER).exists() {
        return Err("the last walk skipped names that are not valid repo paths".into());
    }
    let cache = load_cache(&bee);
    let answer = link
        .local(
            "Changes",
            &json!({ "root": root.display().to_string(), "sinceNs": cache.clock_ns }),
        )
        .map_err(|e| format!("Changes failed: {}", e.message))?;
    if answer["fresh"].as_bool().unwrap_or(true) {
        return Err(format!(
            "the watcher cannot vouch: {}",
            answer["reason"].as_str().unwrap_or("fresh")
        ));
    }
    let state = wc.state();
    let base = wc.manifest();
    let dirty = dirty_paths(&answer)?;
    let under = |path: &[u8]| {
        dirty.iter().any(|d| {
            d.is_empty()
                || path == d.as_bytes()
                || (path.starts_with(d.as_bytes()) && path.get(d.len()) == Some(&b'/'))
        })
    };
    let clock = scan::touch_clock(&bee).map_err(|e| e.message)?;
    let mut seed = cache.clone();
    for d in &dirty {
        seed.entries.remove(d.as_bytes());
    }
    let mut entries = cache.entries.clone();
    entries.retain(|p, _| !under(p));
    if !dirty.is_empty() {
        let profile = Sparse::from_prefixes(dirty.iter()).map_err(|e| e.message)?;
        let partial = scan::scan(root, clock, &profile, base, &state.hidden_set(), &seed)
            .map_err(|e| e.message)?;
        if !partial.skipped.is_empty() {
            return Err("a rescanned name is not a valid repo path".into());
        }
        for (p, e) in partial.cache.entries {
            if state.sparse.contains(&p) {
                entries.insert(p, e);
            }
        }
    }
    let mut actual: Manifest = Manifest::new();
    for (p, (_, e)) in &entries {
        actual.insert(p.clone(), *e);
    }
    let hidden = state.hidden_set();
    for (p, e) in base {
        if (hidden.contains(p) || e.mode == Mode::Gitlink) && !actual.contains_key(p) {
            actual.insert(p.clone(), *e);
        }
    }
    let refreshed = StatCache {
        clock_ns: clock,
        entries,
    };
    fsutil::atomic_write(&bee.join("statcache"), &refreshed.encode()).map_err(|e| e.message)?;
    let changes = manifest::diff(base, &actual);
    trace(&format!(
        "status from the watcher: {} changed path(s) rescanned",
        dirty.len()
    ));
    Ok(report_of(wc, &changes, Vec::new()))
}

/// The directories and files to rescan for a `Changes` answer. A changed `.gitignore` can hide
/// or reveal anything in its directory, so it dirties the directory; a path that is not a
/// valid repo path cannot be rescanned selectively.
fn dirty_paths(answer: &Value) -> Result<Vec<String>, String> {
    let mut dirty: BTreeSet<String> = BTreeSet::new();
    for p in answer["paths"].as_array().cloned().unwrap_or_default() {
        let p = p.as_str().unwrap_or_default().to_owned();
        let p = match p.strip_suffix(".gitignore") {
            Some(dir) if dir.is_empty() || dir.ends_with('/') => {
                dir.trim_end_matches('/').to_owned()
            }
            _ => p,
        };
        if p.is_empty() {
            return Ok(vec![String::new()]);
        }
        if RepoPath::new(p.as_bytes().to_vec()).is_err() {
            return Err(format!("changed path {p:?} is not a valid repo path"));
        }
        dirty.insert(p);
    }
    Ok(dirty.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecars_sit_next_to_the_socket() {
        let s = Path::new("/run/bee/daemon.sock");
        assert_eq!(pid_file(s), Path::new("/run/bee/daemon.sock.pid"));
        assert_eq!(upstream_file(s), Path::new("/run/bee/daemon.sock.upstream"));
        assert_eq!(log_file(s), Path::new("/run/bee/daemon.sock.log"));
    }

    #[test]
    fn endpoints_compare_without_trailing_slashes() {
        assert!(same_endpoint("http://h:1/", "http://h:1\n"));
        assert!(!same_endpoint("http://h:1", "http://h:2"));
    }

    #[test]
    fn gitignore_changes_dirty_their_directory() {
        let answer = json!({"paths": ["a/.gitignore", "b/c.txt", "a/x"]});
        assert_eq!(
            dirty_paths(&answer).expect("paths"),
            vec!["a".to_owned(), "a/x".to_owned(), "b/c.txt".to_owned()]
        );
        let root = json!({"paths": [".gitignore", "z"]});
        assert_eq!(dirty_paths(&root).expect("root"), vec![String::new()]);
        assert!(dirty_paths(&json!({"paths": ["a//b"]})).is_err());
    }

    #[test]
    fn the_daemon_can_be_switched_off() {
        assert!(!enabled(true));
    }
}
