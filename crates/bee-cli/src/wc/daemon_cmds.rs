//! Commands that use the node daemon: `bee daemon start|stop|status|stats|prefetch|pin|unpin`,
//! `bee checkout --virtual` and `bee snapshot`. The daemon itself is `bee-daemon`
//! (`docs/spec/cache-tiers.md`); routing of ordinary commands through it lives in
//! [`super::CliTransport`] and [`crate::daemon_glue`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bee_wc::{CreateOptions, Sparse};
use clap::{Arg, ArgAction, ArgMatches};
use serde_json::{Value, json};

use super::history::{Scope, repo_arg, scope};
use super::{arg, args_of, cli_error, data, flag, open, repo_paths, schema};
use crate::commands::Command;
use crate::context::Context;
use crate::daemon_glue::{self, Link, VIRTUAL_PREFIX};
use crate::error::CliError;
use bee_proto::v1::ErrorReason;

/// Every command that uses the daemon.
pub fn all() -> Vec<&'static dyn Command> {
    vec![
        &DaemonStart,
        &DaemonStop,
        &DaemonStatus,
        &DaemonStats,
        &DaemonPrefetch,
        &DaemonPin,
        &DaemonUnpin,
        &Checkout,
        &Snapshot,
    ]
}

fn socket_arg() -> Arg {
    Arg::new("socket")
        .long("socket")
        .value_name("PATH")
        .help("Daemon socket. Default: BEE_DAEMON_SOCKET, then the per-user default.")
}

fn socket_of(args: &ArgMatches) -> PathBuf {
    daemon_glue::socket_path(arg(args, "socket").as_deref())
}

fn detail(d: &bee_proto::v1::ErrorDetail) -> CliError {
    CliError::from_detail(d, "the daemon refused the request")
}

fn no_daemon(socket: &Path) -> CliError {
    CliError::unavailable(format!(
        "no daemon answers on {}; start one with `bee daemon start`",
        socket.display()
    ))
}

/// The daemon on `--socket` (or the default); every daemon command except `start` needs it.
fn link_for(ctx: &Context, args: &ArgMatches) -> Result<Link, CliError> {
    let socket = socket_of(args);
    Link::connect(&socket, ctx.token()?.as_deref(), None).ok_or_else(|| no_daemon(&socket))
}

fn read_pid(socket: &Path) -> Option<u32> {
    std::fs::read_to_string(daemon_glue::pid_file(socket))
        .ok()
        .and_then(|t| t.trim().parse().ok())
}

fn read_upstream(socket: &Path) -> Option<String> {
    std::fs::read_to_string(daemon_glue::upstream_file(socket))
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

fn describe(data: &Value) -> String {
    let text = |k: &str| data[k].as_str().unwrap_or_default().to_owned();
    let mut out = format!(
        "daemon {} on {}",
        if data["running"].as_bool() == Some(true) {
            "running"
        } else {
            "not running"
        },
        text("socket")
    );
    if let Some(pid) = data["pid"].as_u64() {
        out.push_str(&format!(" (pid {pid})"));
    }
    if let Some(up) = data["upstream"].as_str() {
        out.push_str(&format!(", fronting {up}"));
    }
    out
}

/// `bee daemon start`.
pub struct DaemonStart;

impl Command for DaemonStart {
    fn path(&self) -> &'static [&'static str] {
        &["daemon", "start"]
    }

    fn about(&self) -> &'static str {
        "Start the node daemon (a shared object cache) in the background, unless one runs."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Spawn `bee-daemon serve` detached, in front of the endpoint (--upstream, \
                 else the usual endpoint resolution), and wait until it answers on its socket. \
                 Working copies on this machine then fetch objects through it, so the second \
                 clone of a repo downloads nothing. The binary is $BEE_DAEMON_BIN, else \
                 `bee-daemon` next to `bee`, else the one on PATH. Its log is \
                 <socket>.log. Does nothing if a daemon already answers on the socket.",
            )
            .arg(socket_arg())
            .arg(
                Arg::new("upstream")
                    .long("upstream")
                    .value_name("URL")
                    .help("Server to front. Default: the endpoint."),
            )
            .arg(
                Arg::new("cache")
                    .long("cache")
                    .value_name("DIR")
                    .help("Cache directory. Default: BEE_DAEMON_CACHE, then ~/.bee/cache."),
            )
            .arg(
                Arg::new("max-bytes")
                    .long("max-bytes")
                    .value_name("N")
                    .help("Cache size bound in bytes."),
            )
            .arg(
                Arg::new("no-fsync")
                    .long("no-fsync")
                    .action(ArgAction::SetTrue)
                    .help("Skip fsync of cache files (faster, a crash may lose recent objects)."),
            )
            .after_help("EXAMPLE:\n  bee daemon start --json")
    }

    fn output_schema(&self) -> Value {
        schema::daemon_start()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let socket = socket_of(args);
        let token = ctx.token()?;
        if Link::connect(&socket, token.as_deref(), None).is_some() {
            return Ok(Some(json!({
                "running": true,
                "started": false,
                "socket": socket.display().to_string(),
                "pid": read_pid(&socket),
                "upstream": read_upstream(&socket),
                "log": Value::Null,
            })));
        }
        let upstream = match arg(args, "upstream") {
            Some(u) => u,
            None => ctx.endpoint()?.to_string(),
        };
        let child_args = daemon_args(args, &upstream, &socket);
        let log = daemon_glue::log_file(&socket);
        let pid = spawn_daemon(&child_args, &socket, &log)?;
        let _ = std::fs::write(daemon_glue::pid_file(&socket), pid.to_string());
        let _ = std::fs::write(daemon_glue::upstream_file(&socket), &upstream);
        wait_ready(&socket, token.as_deref(), pid, &log)?;
        Ok(Some(json!({
            "running": true,
            "started": true,
            "socket": socket.display().to_string(),
            "pid": pid,
            "upstream": upstream,
            "log": log.display().to_string(),
        })))
    }

    fn render(&self, data: &Value) -> String {
        describe(data)
    }
}

fn daemon_args(args: &ArgMatches, upstream: &str, socket: &Path) -> Vec<String> {
    let mut out = vec![
        "serve".to_owned(),
        "--upstream".to_owned(),
        upstream.to_owned(),
        "--socket".to_owned(),
        socket.display().to_string(),
    ];
    for (flag, id) in [("--cache", "cache"), ("--max-bytes", "max-bytes")] {
        if let Some(v) = arg(args, id) {
            out.push(flag.to_owned());
            out.push(v);
        }
    }
    if matches!(args.try_get_one::<bool>("no-fsync"), Ok(Some(true))) {
        out.push("--no-fsync".to_owned());
    }
    out
}

/// The `bee-daemon` executable: `BEE_DAEMON_BIN`, the sibling of this executable, or PATH.
fn daemon_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("BEE_DAEMON_BIN").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("bee-daemon");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("bee-daemon")
}

#[cfg(unix)]
fn spawn_daemon(child_args: &[String], socket: &Path, log: &Path) -> Result<u32, CliError> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command as Process, Stdio};

    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::internal(format!("cannot create {}: {e}", dir.display())))?;
    }
    // A socket nobody answers on is a leftover of a crashed daemon.
    let _ = std::fs::remove_file(socket);
    let log_out = std::fs::File::create(log)
        .map_err(|e| CliError::internal(format!("cannot create {}: {e}", log.display())))?;
    let bin = daemon_binary();
    let child = Process::new(&bin)
        .args(child_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_out))
        // Its own process group: Ctrl-C in this terminal must not stop the daemon.
        .process_group(0)
        .spawn()
        .map_err(|e| {
            CliError::unavailable(format!(
                "cannot start {}: {e}; set BEE_DAEMON_BIN to the bee-daemon executable",
                bin.display()
            ))
        })?;
    Ok(child.id())
}

#[cfg(not(unix))]
fn spawn_daemon(_: &[String], _: &Path, _: &Path) -> Result<u32, CliError> {
    Err(CliError::unavailable(
        "the node daemon needs unix sockets; use `bee-daemon regional` instead",
    ))
}

fn wait_ready(socket: &Path, token: Option<&str>, pid: u32, log: &Path) -> Result<(), CliError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if Link::connect(socket, token, None).is_some() {
            return Ok(());
        }
        if !process_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let tail = std::fs::read_to_string(log).unwrap_or_default();
    let tail: String = tail.lines().rev().take(5).collect::<Vec<_>>().join(" | ");
    Err(CliError::unavailable(format!(
        "the daemon did not come up on {} ({tail})",
        socket.display()
    )))
}

fn process_alive(pid: u32) -> bool {
    // `ps` exits 0 only for a live process; a zombie (a daemon that exited and was not
    // reaped yet) shows state Z.
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).starts_with('Z'))
}

fn is_daemon_process(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("bee-daemon"))
}

fn signal(pid: u32, name: &str) {
    let _ = std::process::Command::new("kill")
        .args([name, &pid.to_string()])
        .status();
}

/// `bee daemon stop`.
pub struct DaemonStop;

impl Command for DaemonStop {
    fn path(&self) -> &'static [&'static str] {
        &["daemon", "stop"]
    }

    fn about(&self) -> &'static str {
        "Stop the node daemon that `bee daemon start` started."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Ask the daemon to shut down (SIGINT, then SIGTERM after 5s) and remove its \
                 socket and bookkeeping files. Works on daemons started by `bee daemon \
                 start` (their pid is recorded next to the socket); for one started by hand, \
                 stop the process yourself. Succeeds with `stopped: false` when none runs.",
            )
            .arg(socket_arg())
            .after_help("EXAMPLE:\n  bee daemon stop --json")
    }

    fn output_schema(&self) -> Value {
        schema::daemon_stop()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let socket = socket_of(args);
        let token = ctx.token()?;
        let alive = Link::connect(&socket, token.as_deref(), None).is_some();
        let pid = read_pid(&socket);
        let report = |stopped: bool| {
            Some(json!({
                "stopped": stopped,
                "socket": socket.display().to_string(),
                "pid": pid,
            }))
        };
        let cleanup = || {
            for f in [
                daemon_glue::pid_file(&socket),
                daemon_glue::upstream_file(&socket),
            ] {
                let _ = std::fs::remove_file(f);
            }
        };
        if !alive {
            cleanup();
            return Ok(report(false));
        }
        let Some(pid) = pid.filter(|p| process_alive(*p) && is_daemon_process(*p)) else {
            return Err(CliError::new(
                ErrorReason::InvalidState,
                format!(
                    "the daemon on {} was not started by `bee daemon start`; stop its \
                     process directly",
                    socket.display()
                ),
            )
            .with_payload("subcode", json!("daemon_not_ours")));
        };
        signal(pid, "-INT");
        let down = |wait: Duration| {
            let end = Instant::now() + wait;
            while Instant::now() < end {
                if !process_alive(pid) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            !process_alive(pid)
        };
        if !down(Duration::from_secs(5)) {
            signal(pid, "-TERM");
            if !down(Duration::from_secs(5)) {
                return Err(CliError::internal(format!(
                    "the daemon (pid {pid}) did not stop"
                )));
            }
        }
        let _ = std::fs::remove_file(&socket);
        cleanup();
        Ok(report(true))
    }

    fn render(&self, data: &Value) -> String {
        if data["stopped"].as_bool() == Some(true) {
            format!(
                "daemon stopped ({})",
                data["socket"].as_str().unwrap_or_default()
            )
        } else {
            format!(
                "no daemon was running on {}",
                data["socket"].as_str().unwrap_or_default()
            )
        }
    }
}

/// `bee daemon status`.
pub struct DaemonStatus;

impl Command for DaemonStatus {
    fn path(&self) -> &'static [&'static str] {
        &["daemon", "status"]
    }

    fn about(&self) -> &'static str {
        "Show whether a node daemon answers on the socket."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Probe the socket. `pid` and `upstream` are known for daemons started by \
                 `bee daemon start`. Exits 0 either way; read `running`.",
            )
            .arg(socket_arg())
            .after_help("EXAMPLE:\n  bee daemon status --json")
    }

    fn output_schema(&self) -> Value {
        schema::daemon_status()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let socket = socket_of(args);
        let link = Link::connect(&socket, ctx.token()?.as_deref(), None);
        let tier = link
            .as_ref()
            .and_then(|l| l.local("Stats", &json!({})).ok())
            .and_then(|s| s["tier"].as_str().map(str::to_owned));
        Ok(Some(json!({
            "running": link.is_some(),
            "socket": socket.display().to_string(),
            "pid": if link.is_some() { read_pid(&socket) } else { None },
            "upstream": if link.is_some() { read_upstream(&socket) } else { None },
            "tier": tier,
        })))
    }

    fn render(&self, data: &Value) -> String {
        describe(data)
    }
}

/// `bee daemon stats`.
pub struct DaemonStats;

impl Command for DaemonStats {
    fn path(&self) -> &'static [&'static str] {
        &["daemon", "stats"]
    }

    fn about(&self) -> &'static str {
        "Show the node daemon's counters: hits, misses, upstream fetches, cache size."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "The daemon's counters since it started. `upstreamObjects` is how many object \
                 ids it had to fetch from the server: a second clone of the same repo leaves \
                 it unchanged.",
            )
            .arg(socket_arg())
            .after_help("EXAMPLE:\n  bee daemon stats --json")
    }

    fn output_schema(&self) -> Value {
        schema::daemon_stats()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let link = link_for(ctx, args)?;
        link.local("Stats", &json!({}))
            .map(Some)
            .map_err(|d| detail(&d))
    }

    fn render(&self, data: &Value) -> String {
        format!(
            "tier {}: {} hits, {} misses (hit rate {:.2}), {} upstream request(s) for {} \
             object(s), {} object(s) cached ({} bytes), {} pinned",
            data["tier"].as_str().unwrap_or_default(),
            data["hits"],
            data["misses"],
            data["hitRate"].as_f64().unwrap_or(0.0),
            data["upstreamRequests"],
            data["upstreamObjects"],
            data["store"]["objects"],
            data["store"]["bytes"],
            data["store"]["pinned"]
        )
    }
}

fn revision_arg() -> Arg {
    Arg::new("rev")
        .long("rev")
        .value_name("REV")
        .help("Revision (ref, changeset id). Default: the working copy's target ref.")
}

fn scope_request(
    sc: &Scope,
    args: &ArgMatches,
    given: &[String],
) -> Result<serde_json::Map<String, Value>, CliError> {
    let mut paths = match &sc.root {
        Some(root) => repo_paths(root, given)?,
        None => given.to_vec(),
    };
    // `.` is the whole repo, which the daemon spells as no prefix at all.
    if paths.iter().any(|p| p == "." || p.is_empty()) {
        paths.clear();
    }
    let mut request = serde_json::Map::new();
    request.insert("repo".into(), json!(sc.repo));
    request.insert(
        "revision".into(),
        json!(arg(args, "rev").unwrap_or_else(|| sc.target_ref.clone())),
    );
    request.insert("sparse".into(), json!(paths));
    Ok(request)
}

fn paths_positional(help: &'static str) -> Arg {
    Arg::new("paths")
        .value_name("PATH")
        .num_args(0..)
        .action(ArgAction::Append)
        .help(help)
}

/// `bee daemon prefetch`.
pub struct DaemonPrefetch;

impl Command for DaemonPrefetch {
    fn path(&self) -> &'static [&'static str] {
        &["daemon", "prefetch"]
    }

    fn about(&self) -> &'static str {
        "Load the objects under PATHs into the daemon's cache before anything needs them."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Fetch every object readable by you under the PATHs (default: the whole repo) \
                 at --rev into the node daemon, so later clones, checkouts and snapshots of \
                 them download nothing. Paths are relative to the current directory inside a \
                 working copy, else repo paths. `fetched` counts what had to come from the \
                 server.",
            )
            .arg(socket_arg())
            .arg(repo_arg())
            .arg(revision_arg())
            .arg(paths_positional("Paths (prefixes) to prefetch."))
            .after_help("EXAMPLE:\n  bee daemon prefetch services/api --json")
    }

    fn output_schema(&self) -> Value {
        schema::snapshot_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let link = link_for(ctx, args)?;
        let sc = scope(ctx, args)?;
        let request = scope_request(&sc, args, &args_of(args, "paths"))?;
        link.local("Prefetch", &Value::Object(request))
            .map(Some)
            .map_err(|d| detail(&d))
    }

    fn render(&self, data: &Value) -> String {
        format!(
            "{}: {} file(s), {} byte(s), {} object(s) fetched from the server",
            data["changeset"].as_str().unwrap_or_default(),
            data["files"],
            data["bytes"],
            data["fetched"]
        )
    }
}

/// `bee daemon pin`.
pub struct DaemonPin;

impl Command for DaemonPin {
    fn path(&self) -> &'static [&'static str] {
        &["daemon", "pin"]
    }

    fn about(&self) -> &'static str {
        "Keep the objects under PATHs in the daemon's cache under a name (never evicted)."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Prefetch the objects under the PATHs and pin them as the set NAME: the cache \
                 never evicts them until `bee daemon unpin NAME`. Pinning an existing name \
                 replaces its set.",
            )
            .arg(socket_arg())
            .arg(repo_arg())
            .arg(revision_arg())
            .arg(
                Arg::new("name")
                    .value_name("NAME")
                    .required(true)
                    .help("Name of the pinned set."),
            )
            .arg(paths_positional("Paths (prefixes) to pin."))
            .after_help("EXAMPLE:\n  bee daemon pin toolchain tools/ --json")
    }

    fn output_schema(&self) -> Value {
        schema::daemon_pin()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let link = link_for(ctx, args)?;
        let sc = scope(ctx, args)?;
        let mut request = scope_request(&sc, args, &args_of(args, "paths"))?;
        request.insert("name".into(), json!(arg(args, "name").unwrap_or_default()));
        link.local("Pin", &Value::Object(request))
            .map(Some)
            .map_err(|d| detail(&d))
    }

    fn render(&self, data: &Value) -> String {
        format!(
            "pinned {} ({} object(s) at {})",
            data["name"].as_str().unwrap_or_default(),
            data["objects"],
            data["changeset"].as_str().unwrap_or_default()
        )
    }
}

/// `bee daemon unpin`.
pub struct DaemonUnpin;

impl Command for DaemonUnpin {
    fn path(&self) -> &'static [&'static str] {
        &["daemon", "unpin"]
    }

    fn about(&self) -> &'static str {
        "Release a pinned set; its objects become evictable again."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .arg(socket_arg())
            .arg(
                Arg::new("name")
                    .value_name("NAME")
                    .required(true)
                    .help("Name given to `bee daemon pin`."),
            )
            .after_help("EXAMPLE:\n  bee daemon unpin toolchain --json")
    }

    fn output_schema(&self) -> Value {
        schema::daemon_unpin()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let link = link_for(ctx, args)?;
        link.local(
            "Unpin",
            &json!({ "name": arg(args, "name").unwrap_or_default() }),
        )
        .map(Some)
        .map_err(|d| detail(&d))
    }

    fn render(&self, data: &Value) -> String {
        if data["removed"].as_bool() == Some(true) {
            "unpinned".to_owned()
        } else {
            "no such pinned set".to_owned()
        }
    }
}

/// `bee checkout --virtual`.
pub struct Checkout;

impl Command for Checkout {
    fn path(&self) -> &'static [&'static str] {
        &["checkout"]
    }

    fn about(&self) -> &'static str {
        "Move to a revision (like `goto`), or with --virtual make a working copy on demand."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Without --virtual this is `bee goto REV`. With --virtual it makes a new \
                 working copy of REPO in DIR, sparse-on-demand (docs/spec/cache-tiers.md §7). \
                 The working copy \
                 starts with an empty sparse profile (`.bee-virtual`, a prefix that names \
                 nothing), so no file is written. The whole tree stays readable through the \
                 node daemon's cache (`bee cat`, `bee daemon prefetch`); touching a path \
                 materializes it: `bee sparse add PATH` writes it (fetched through the \
                 daemon), after which `status`, `commit`, `sync` and `land` treat it like any \
                 file. Commits are deltas from their parent, so every untouched path keeps its \
                 content on the server. Paths named with --touch are materialized right away. \
                 Without a daemon the checkout still works; touched paths are then fetched \
                 from the server.",
            )
            .arg(
                Arg::new("virtual")
                    .long("virtual")
                    .action(ArgAction::SetTrue)
                    .help("Make a new working copy on demand instead of moving to a revision."),
            )
            .arg(
                Arg::new("repo")
                    .value_name("REPO_OR_REV")
                    .required(true)
                    .help("With --virtual the repo as org/name; otherwise the revision to go to."),
            )
            .arg(
                Arg::new("discard-stack")
                    .long("discard-stack")
                    .action(ArgAction::SetTrue)
                    .help("Without --virtual: allow leaving unlanded changes behind."),
            )
            .arg(
                Arg::new("directory")
                    .value_name("DIR")
                    .help("Directory to create."),
            )
            .arg(
                Arg::new("ref")
                    .long("ref")
                    .value_name("REF")
                    .default_value("main")
                    .help("Protected ref to sync with and land on."),
            )
            .arg(
                Arg::new("touch")
                    .long("touch")
                    .value_name("PATH")
                    .action(ArgAction::Append)
                    .help("Materialize PATH now (repeatable)."),
            )
            .after_help(
                "EXAMPLE:\n  bee checkout --virtual acme/mono mono --touch services/api/main.rs\n  \
                 bee checkout main~1",
            )
    }

    fn output_schema(&self) -> Value {
        schema::op_report()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        if !flag(args, "virtual") {
            let (mut wc, t) = open(ctx)?;
            let rev = arg(args, "repo").unwrap_or_default();
            let report = wc
                .goto(&t, &rev, flag(args, "discard-stack"))
                .map_err(cli_error)?;
            return data(&report);
        }
        let mut prefixes = vec![VIRTUAL_PREFIX.to_owned()];
        prefixes.extend(args_of(args, "touch"));
        let sparse = Sparse::from_prefixes(&prefixes).map_err(cli_error)?;
        super::commands::create_with(ctx, args, false, Some(sparse))
    }

    fn render(&self, data: &Value) -> String {
        super::commands::render_op(data)
    }
}

/// `bee snapshot`.
pub struct Snapshot;

impl Command for Snapshot {
    fn path(&self) -> &'static [&'static str] {
        &["snapshot"]
    }

    fn about(&self) -> &'static str {
        "Materialize a revision into DIR through the node daemon, as a working copy."
    }

    fn configure(&self, command: clap::Command) -> clap::Command {
        command
            .long_about(
                "Ask the node daemon to write the files of REV into DIR (copy-on-write clones \
                 of its cache files, or hard links with --mode hardlink), then bind DIR to \
                 the repo's --ref as `bee init` does, keeping the files in place: nothing \
                 more is fetched. This is how CI jobs and agents get a checkout in seconds. \
                 REV is a ref, changeset id or any revision expression; if it is not the tip \
                 of --ref, `bee status` in DIR shows the difference (`matchesHead` says \
                 which). Needs a running daemon (`bee daemon start`) and a DIR that does not \
                 exist or is empty.",
            )
            .arg(repo_arg())
            .arg(
                Arg::new("revision")
                    .value_name("REV")
                    .required(true)
                    .help("Revision to materialize."),
            )
            .arg(
                Arg::new("directory")
                    .value_name("DIR")
                    .required(true)
                    .help("Directory to fill."),
            )
            .arg(
                Arg::new("ref")
                    .long("ref")
                    .value_name("REF")
                    .default_value("main")
                    .help("Protected ref the working copy syncs with and lands on."),
            )
            .arg(
                Arg::new("sparse")
                    .long("sparse")
                    .value_name("PREFIX")
                    .action(ArgAction::Append)
                    .help("Materialize only paths under PREFIX (repeatable)."),
            )
            .arg(
                Arg::new("mode")
                    .long("mode")
                    .value_name("MODE")
                    .value_parser(["clone", "hardlink", "copy"])
                    .default_value("clone")
                    .help("How files are created from the cache."),
            )
            .arg(
                Arg::new("verify")
                    .long("verify")
                    .action(ArgAction::SetTrue)
                    .help("Re-read every written file and check it against its id."),
            )
            .after_help("EXAMPLE:\n  bee snapshot main /work/job-17 --repo acme/app --json")
    }

    fn output_schema(&self) -> Value {
        schema::snapshot()
    }

    fn run(&self, ctx: &mut Context, args: &ArgMatches) -> Result<Option<Value>, CliError> {
        let sc = scope(ctx, args)?;
        let socket = daemon_glue::socket_path(None);
        let link = if daemon_glue::enabled(ctx.options.no_daemon) {
            sc.transport.daemon()
        } else {
            None
        };
        let Some(link) = link else {
            return Err(CliError::unavailable(format!(
                "`bee snapshot` needs the node daemon: nothing answers on {} (or it is \
                 disabled); start one with `bee daemon start`",
                socket.display()
            )));
        };
        let cwd = std::env::current_dir().map_err(|e| CliError::internal(e.to_string()))?;
        let dir = cwd.join(arg(args, "directory").unwrap_or_default());
        let revision = arg(args, "revision").unwrap_or_default();
        let sparse_prefixes = args_of(args, "sparse");
        let request = json!({
            "repo": sc.repo,
            "revision": revision,
            "dest": dir.display().to_string(),
            "sparse": sparse_prefixes,
            "mode": arg(args, "mode").unwrap_or_else(|| "clone".into()),
            "verify": flag(args, "verify"),
        });
        let mut report = link.local("Snapshot", &request).map_err(|d| detail(&d))?;
        let opts = CreateOptions {
            endpoint: sc.transport.endpoint(),
            repo: sc.repo.clone(),
            target_ref: arg(args, "ref").unwrap_or_else(|| "main".into()),
            sparse: Sparse::from_prefixes(&sparse_prefixes).map_err(cli_error)?,
            seed: None,
            adopt: true,
        };
        let open = bee_wc::OpenOptions {
            faults: bee_wc::Faults::from_env(),
            ..bee_wc::OpenOptions::default()
        };
        let (wc, _) = bee_wc::create(&dir, &opts, &sc.transport, &open).map_err(cli_error)?;
        let head = wc.state().head.to_text();
        drop(wc);
        link.watch(&dir);
        let changeset = report["changeset"].as_str().unwrap_or_default().to_owned();
        let Some(map) = report.as_object_mut() else {
            return Err(CliError::internal(
                "the daemon sent a malformed snapshot report",
            ));
        };
        map.insert("repo".into(), json!(sc.repo));
        map.insert("revision".into(), json!(revision));
        map.insert("dir".into(), json!(dir.display().to_string()));
        map.insert("matchesHead".into(), json!(changeset == head));
        map.insert("head".into(), json!(head));
        Ok(Some(report))
    }

    fn render(&self, data: &Value) -> String {
        format!(
            "{} at {}: {} file(s), {} byte(s) in {}; {} object(s) fetched from the server{}",
            data["repo"].as_str().unwrap_or_default(),
            data["changeset"].as_str().unwrap_or_default(),
            data["files"],
            data["bytes"],
            data["dir"].as_str().unwrap_or_default(),
            data["fetched"],
            if data["matchesHead"].as_bool() == Some(true) {
                ""
            } else {
                "\nnote: this is not the tip of the ref; `bee status` shows the difference"
            }
        )
    }
}
