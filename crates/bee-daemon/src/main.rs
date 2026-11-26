//! `bee-daemon`: the node daemon, the regional cache, and a small client for both.
//!
//! ```text
//! bee-daemon serve    [--config FILE] --upstream URL [--socket PATH] [--cache DIR] [--max-bytes N]
//!                       [--no-fsync] [--no-signed-urls] [--confirm-ms N] [--ref-ttl-ms N]
//!                       [--upstream-concurrency N] [--bootstrap ORG/REPO]...
//!                       [--bootstrap-sparse PREFIX]... [--bootstrap-every-secs N] [--print-config]
//! bee-daemon regional [--config FILE] --upstream URL --listen ADDR [--memory-bytes N]
//!                       [--no-signed-urls] [--print-config]
//! bee-daemon call OP [JSON] [--socket PATH]     # /daemon/v1/OP, e.g. Stats, Snapshot
//! bee-daemon stats [--socket PATH]
//! ```
//!
//! Client commands send `BEE_TOKEN` as the bearer credential and print one JSON document.
//! Server settings come from defaults, then the configuration file (`--config`, else
//! `$BEE_DAEMON_CONFIG`; [`bee_daemon::config`]), then the environment, then flags.

use std::path::PathBuf;
use std::process::ExitCode;

use bee_daemon::config::Settings;
use bee_daemon::{
    BlockingClient, Endpoint, TierKind, daemon_tier, default_socket_path, regional_tier,
};

struct Args {
    rest: Vec<String>,
}

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        match self.rest.iter().position(|a| a == name) {
            Some(i) => {
                self.rest.remove(i);
                true
            }
            None => false,
        }
    }

    fn value(&mut self, name: &str) -> Result<Option<String>, String> {
        match self.rest.iter().position(|a| a == name) {
            Some(i) if i + 1 < self.rest.len() => {
                let v = self.rest.remove(i + 1);
                self.rest.remove(i);
                Ok(Some(v))
            }
            Some(_) => Err(format!("{name} needs a value")),
            None => Ok(None),
        }
    }

    fn number(&mut self, name: &str) -> Result<Option<u64>, String> {
        self.value(name)?
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|_| format!("{name}: not a number: {v}"))
            })
            .transpose()
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  bee-daemon serve [--config FILE] [--print-config] --upstream URL [--socket PATH] [--cache DIR] [--max-bytes N] [--no-fsync] [--no-signed-urls] [--confirm-ms N] [--ref-ttl-ms N] [--bootstrap ORG/REPO]... [--bootstrap-sparse P]... [--bootstrap-every-secs N]\n  bee-daemon regional [--config FILE] [--print-config] --upstream URL --listen ADDR [--memory-bytes N] [--no-signed-urls]\n  bee-daemon call OP [JSON] [--socket PATH]\n  bee-daemon stats [--socket PATH]"
    );
    ExitCode::from(2)
}

/// Settings: defaults, the file, the environment, then the flags in `args`.
fn settings(regional: bool, args: &mut Args) -> Result<Settings, String> {
    let file = args.value("--config")?.or_else(|| {
        std::env::var("BEE_DAEMON_CONFIG")
            .ok()
            .filter(|v| !v.is_empty())
    });
    let text = match &file {
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?,
        None => String::new(),
    };
    let mut s = Settings::load(&text, &|name| std::env::var(name).ok())?;
    if let Some(v) = args.value("--upstream")? {
        s.upstream = Some(v);
    }
    if args.flag("--no-signed-urls") {
        s.signed_urls = false;
    }
    if let Some(n) = args.number("--confirm-ms")? {
        s.confirm_ms = n;
    }
    if let Some(n) = args.number("--ref-ttl-ms")? {
        s.ref_ttl_ms = n;
    }
    if let Some(n) = args.number("--upstream-concurrency")? {
        s.upstream_concurrency = n as usize;
    }
    if regional {
        if let Some(v) = args.value("--listen")? {
            s.listen = Some(v);
        }
        if let Some(n) = args.number("--memory-bytes")? {
            s.memory_bytes = n;
        }
    } else {
        if let Some(v) = args.value("--socket")? {
            s.socket = PathBuf::from(v);
        }
        if let Some(v) = args.value("--cache")? {
            s.cache_dir = PathBuf::from(v);
        }
        if let Some(n) = args.number("--max-bytes")? {
            s.max_bytes = n;
        }
        if args.flag("--no-fsync") {
            s.fsync = false;
        }
        let mut repos = Vec::new();
        while let Some(r) = args.value("--bootstrap")? {
            repos.push(r);
        }
        if !repos.is_empty() {
            s.bootstrap = repos;
        }
        let mut sparse = Vec::new();
        while let Some(p) = args.value("--bootstrap-sparse")? {
            sparse.push(p);
        }
        if !sparse.is_empty() {
            s.bootstrap_sparse = sparse;
        }
        if let Some(n) = args.number("--bootstrap-every-secs")? {
            s.bootstrap_every_secs = n;
        }
    }
    s.validate()?;
    Ok(s)
}

fn settings_json(s: &Settings) -> serde_json::Value {
    serde_json::json!({
        "upstream": s.upstream,
        "socket": s.socket.display().to_string(),
        "listen": s.listen,
        "cache_dir": s.cache_dir.display().to_string(),
        "max_bytes": s.max_bytes,
        "low_watermark_pct": s.low_watermark_pct,
        "fsync": s.fsync,
        "memory_bytes": s.memory_bytes,
        "signed_urls": s.signed_urls,
        "confirm_ms": s.confirm_ms,
        "ref_ttl_ms": s.ref_ttl_ms,
        "upstream_concurrency": s.upstream_concurrency,
        "max_retries": s.max_retries,
        "bootstrap": s.bootstrap,
        "bootstrap_sparse": s.bootstrap_sparse,
        "bootstrap_every_secs": s.bootstrap_every_secs,
    })
}

fn run_server(cmd: &str, mut args: Args) -> Result<(), String> {
    let regional = cmd == "regional";
    let print = args.flag("--print-config");
    let s = settings(regional, &mut args)?;
    reject_extra(&args)?;
    if print {
        println!(
            "{}",
            serde_json::to_string_pretty(&settings_json(&s)).unwrap_or_default()
        );
        return Ok(());
    }
    let upstream = s
        .upstream
        .clone()
        .ok_or("--upstream (or BEE_ENDPOINT, or `upstream` in the config) is required")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let running = if !regional {
            let tier = daemon_tier(s.disk(), &upstream, s.tier(TierKind::Daemon))
                .map_err(|e| e.message.clone())?;
            if !s.bootstrap.is_empty() {
                spawn_bootstraps(
                    tier.clone(),
                    s.bootstrap.clone(),
                    s.bootstrap_sparse.clone(),
                    s.bootstrap_every_secs.max(60),
                );
            }
            serve_socket(tier, &s.socket).await?
        } else {
            let listen = s.listen.clone().ok_or("--listen is required")?;
            let tier = regional_tier(s.memory_bytes, &upstream, s.tier(TierKind::Regional))
                .map_err(|e| e.message.clone())?;
            bee_daemon::server::serve_tcp(tier, &listen)
                .await
                .map_err(|e| e.message.clone())?
        };
        eprintln!("bee-daemon: listening on {}", running.endpoint());
        let _ = tokio::signal::ctrl_c().await;
        running.shutdown().await;
        Ok(())
    })
}

/// Loads each repo's snapshot now and every `every` seconds (PRODUCT §4.5: the daemon starts
/// from periodically refreshed snapshots), as the principal of `BEE_TOKEN`.
fn spawn_bootstraps(
    tier: std::sync::Arc<bee_daemon::Tier>,
    repos: Vec<String>,
    sparse: Vec<String>,
    every: u64,
) {
    let auth = bee_daemon::Auth(
        std::env::var("BEE_TOKEN")
            .ok()
            .map(|t| format!("Bearer {t}")),
    );
    tokio::spawn(async move {
        loop {
            for repo in &repos {
                match bee_daemon::bootstrap::bootstrap(&tier, &auth, repo, &sparse).await {
                    Ok(r) => eprintln!(
                        "bee-daemon: bootstrapped {repo} from {} ({} objects, {} new)",
                        r.snapshot, r.objects, r.loaded
                    ),
                    Err(e) => eprintln!(
                        "bee-daemon: bootstrap of {repo} failed: {}: {}",
                        e.reason, e.message
                    ),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(every)).await;
        }
    });
}

#[cfg(unix)]
async fn serve_socket(
    tier: std::sync::Arc<bee_daemon::Tier>,
    socket: &std::path::Path,
) -> Result<bee_daemon::Running, String> {
    bee_daemon::server::serve_unix(tier, socket)
        .await
        .map_err(|e| e.message.clone())
}

#[cfg(not(unix))]
async fn serve_socket(
    _tier: std::sync::Arc<bee_daemon::Tier>,
    _socket: &std::path::Path,
) -> Result<bee_daemon::Running, String> {
    Err("the node daemon needs unix sockets; run `bee-daemon regional` instead".into())
}

fn reject_extra(args: &Args) -> Result<(), String> {
    match args.rest.first() {
        Some(a) => Err(format!("unexpected argument {a}")),
        None => Ok(()),
    }
}

fn run_client(op: &str, mut args: Args) -> Result<(), String> {
    let socket = args
        .value("--socket")?
        .unwrap_or_else(|| default_socket_path().display().to_string());
    let endpoint = Endpoint::parse(&socket).or_else(|_| Endpoint::parse(&format!("unix:{socket}")));
    let endpoint = endpoint.map_err(|e| e.message.clone())?;
    let body: serde_json::Value = match args.rest.first() {
        Some(j) => serde_json::from_str(j).map_err(|e| format!("bad JSON: {e}"))?,
        None => serde_json::json!({}),
    };
    let token = std::env::var("BEE_TOKEN").ok();
    let client = BlockingClient::new(endpoint, token.as_deref()).map_err(|e| e.message.clone())?;
    match client.local(op, &body) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            Ok(())
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "error": *e }))
                    .unwrap_or_default()
            );
            Err(format!("{}: {}", e.reason, e.message))
        }
    }
}

fn main() -> ExitCode {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        return usage();
    }
    let cmd = argv.remove(0);
    let result = match cmd.as_str() {
        "serve" | "regional" => run_server(&cmd, Args { rest: argv }),
        "stats" => run_client("Stats", Args { rest: argv }),
        "call" if !argv.is_empty() => {
            let op = argv.remove(0);
            run_client(&op, Args { rest: argv })
        }
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bee-daemon: {e}");
            ExitCode::FAILURE
        }
    }
}
