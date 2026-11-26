//! `bee-server`: run the server from a TOML config, or `--dev` with zero external
//! dependencies (SQLite and disk storage under `./.bee-dev`).

use std::path::PathBuf;
use std::process::ExitCode;

use bee_server::config::Config;
use bee_server::server::{ServerOptions, dev_data_dir, start};

const USAGE: &str = "usage: bee-server [--config <file>] [--dev [--temp]] [--listen <addr>] [--data-dir <dir>]

  --config <file>    TOML configuration (unknown keys are rejected; $VAR values come from the environment)
  --dev              development mode: SQLite + disk storage, org `dev`, repo `dev/playground`,
                     a root token for user `dev` printed on start and written to <data-dir>/credentials,
                     and a second human `reviewer` (with its token) to approve dev's changes
  --temp             with --dev, use a fresh temporary data directory instead of ./.bee-dev
  --listen <addr>    listen address (default 127.0.0.1:7420)
  --data-dir <dir>   data directory
  --check-config     validate the configuration and exit
  --check-production run the operational checklist on the configuration, print it and exit
                     (exit 1 when an error-level item fails)";

struct Args {
    config: Option<PathBuf>,
    dev: bool,
    temp: bool,
    listen: Option<String>,
    data_dir: Option<PathBuf>,
    check: bool,
    check_production: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        config: None,
        dev: false,
        temp: false,
        listen: None,
        data_dir: None,
        check: false,
        check_production: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => a.config = Some(it.next().ok_or("--config needs a file")?.into()),
            "--dev" => a.dev = true,
            "--temp" => a.temp = true,
            "--listen" => a.listen = Some(it.next().ok_or("--listen needs an address")?),
            "--data-dir" => {
                a.data_dir = Some(it.next().ok_or("--data-dir needs a directory")?.into())
            }
            "--check-config" => a.check = true,
            "--check-production" => a.check_production = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("bee-server: {msg}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut cfg = match &args.config {
        Some(p) => match Config::load(p) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("bee-server: {e}");
                return ExitCode::from(2);
            }
        },
        None => Config::default(),
    };
    if args.dev {
        let dir = match (&args.data_dir, dev_data_dir(args.temp)) {
            (Some(d), _) => d.clone(),
            (None, Ok(d)) => d,
            (None, Err(e)) => {
                eprintln!("bee-server: cannot create a data directory: {e}");
                return ExitCode::from(1);
            }
        };
        let base = Config::dev(dir, None);
        if args.config.is_none() {
            cfg = base;
        } else {
            cfg.server.data_dir = base.server.data_dir;
            cfg.import.allowed_roots = base.import.allowed_roots;
        }
    } else if let Some(d) = &args.data_dir {
        cfg.server.data_dir = d.clone();
    }
    cfg.resolve_local_runner(args.dev);
    if let Some(l) = args.listen {
        cfg.server.listen = l;
    }
    if let Err(e) = cfg.validate() {
        eprintln!("bee-server: {e}");
        return ExitCode::from(2);
    }
    if let Some(warning) = bee_server::forge::local_runner::startup_warning(
        cfg.forge.local_runner.enabled.unwrap_or(false),
        args.dev,
    ) && (args.check || args.check_production)
    {
        eprintln!("bee-server: WARN: {warning}");
    }
    if args.check {
        println!("config ok");
        return ExitCode::SUCCESS;
    }
    if args.check_production {
        let checks = bee_server::preflight::production_checklist(&cfg);
        for c in &checks {
            let mark = match (c.ok, c.severity) {
                (true, _) => "ok  ",
                (false, bee_server::preflight::Severity::Error) => "FAIL",
                (false, bee_server::preflight::Severity::Warning) => "warn",
            };
            println!("{mark} {:<18} {:<32} {}", c.id, c.key_path, c.message);
        }
        return if bee_server::preflight::passes(&checks) {
            println!("production checklist: ok");
            ExitCode::SUCCESS
        } else {
            println!("production checklist: failed");
            ExitCode::from(1)
        };
    }
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = args.dev;
    opts.write_credentials = args.dev;
    let running = match start(opts).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bee-server: {}: {}", e.detail.reason, e.detail.message);
            return ExitCode::from(1);
        }
    };
    println!("bee-server listening on {}", running.url);
    if let Some(dev) = &running.dev {
        println!(
            "dev mode: org `dev`, repo `dev/playground`, user `dev` ({})",
            dev.user
        );
        println!("endpoint: {}", running.url);
        println!("token:    {}", dev.token);
        println!(
            "reviewer: user `reviewer` ({}), a second human to approve the dev user's changes",
            dev.reviewer
        );
        println!("reviewer token: {}", dev.reviewer_token);
        println!(
            "credentials written to {}",
            cfg.server.data_dir.join("credentials").display()
        );
        println!(
            "try: curl -s -X POST {}/bee.v1.AuthService/WhoAmI -H 'Content-Type: application/json' -H \"Authorization: Bearer $TOKEN\" -d '{{}}'",
            running.url
        );
    }
    // Without a handler, SIGTERM does nothing when the server is PID 1 (every container), so an
    // orchestrator waited out its whole grace period and then killed the process with SIGKILL.
    running.wait_or_stop(termination_signal(), DRAIN).await;
    ExitCode::SUCCESS
}

/// How long requests in flight get to finish after SIGTERM or SIGINT. Streams (event
/// subscriptions) never finish on their own, so this also bounds shutdown.
const DRAIN: std::time::Duration = std::time::Duration::from_secs(20);

/// Completes on SIGTERM or SIGINT; never completes when the handlers cannot be installed.
async fn termination_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    eprintln!("bee-server: stopping");
}
