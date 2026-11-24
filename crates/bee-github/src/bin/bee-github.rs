//! `bee-github`: move a GitHub repository to Beehive.
//!
//! ```text
//! bee-github import <owner>/<repo> [--server URL] [--token T] [--mirror-only] [--resume] [--dry-run] [--json]
//! bee-github sync <owner>/<repo> [--webhook-listen ADDR --webhook-secret S] [--poll-secs N]
//! bee-github cutover <owner>/<repo> [--archive-github]
//! bee-github claim-token <owner>/<repo> <github-user-id>
//! ```
//!
//! GitHub credentials: `--github-token`, `--app-id/--app-key/--installation`, else
//! `GH_TOKEN`/`GITHUB_TOKEN`, else `gh auth token`. The server: `--server`/`--token`, else
//! `BEE_ENDPOINT`/`BEE_TOKEN`. With `--json` the result is one JSON document on stdout;
//! errors are `{"error": {"reason", "message"}}` with a non-zero exit.

use std::path::PathBuf;
use std::sync::Arc;

use bee_core::BeeError;
use bee_github::auth::{AppCredential, Credential, discover_token};
use bee_github::clock::{Clock, SystemClock};
use bee_github::live::{WebhookApp, webhook_router};
use bee_github::server::{ServerTarget, UnwiredSink, rpc, start_github_import};
use bee_github::state::StateDir;
use bee_github::{Client, ClientConfig, Importer, MemorySink, Options};
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Value, json};

fn cli() -> Command {
    let repo = Arg::new("repo").required(true).help("owner/repo on GitHub");
    let common = [
        Arg::new("server")
            .long("server")
            .help("Beehive server URL (default: BEE_ENDPOINT or http://127.0.0.1:7420)"),
        Arg::new("token")
            .long("token")
            .help("Beehive token (default: BEE_TOKEN)"),
        Arg::new("github-token")
            .long("github-token")
            .help("GitHub token (default: GH_TOKEN, GITHUB_TOKEN, `gh auth token`)"),
        Arg::new("app-id")
            .long("app-id")
            .help("GitHub App id (with --app-key and --installation)"),
        Arg::new("app-key")
            .long("app-key")
            .help("GitHub App private key file (PEM)"),
        Arg::new("installation")
            .long("installation")
            .help("GitHub App installation id"),
        Arg::new("api-url")
            .long("api-url")
            .help("GitHub API base (GitHub Enterprise Server or a test fake)"),
        Arg::new("hub-url")
            .long("hub-url")
            .help("Beehive web URL for rewritten links and redirects"),
        Arg::new("org")
            .long("org")
            .help("Beehive org (default: the GitHub owner)"),
        Arg::new("state")
            .long("state")
            .help("state directory (default: ./.bee-github/<owner>/<repo>)"),
        Arg::new("json")
            .long("json")
            .action(ArgAction::SetTrue)
            .help("print JSON"),
    ];
    Command::new("bee-github")
        .about("Move a GitHub repository to Beehive: identical commit ids, same PR and issue numbers, a fidelity report")
        .subcommand_required(true)
        .subcommand(
            Command::new("import")
                .about("Import (or resume importing) a repository")
                .arg(repo.clone())
                .args(common.clone())
                .arg(Arg::new("mirror-only").long("mirror-only").action(ArgAction::SetTrue).help("only the git phase"))
                .arg(Arg::new("resume").long("resume").action(ArgAction::SetTrue).help("continue an interrupted import (always safe; the default when state exists)"))
                .arg(Arg::new("dry-run").long("dry-run").action(ArgAction::SetTrue).help("estimate counts, requests and time; fetch nothing"))
                .arg(Arg::new("archive").long("archive").help("org migration archive (.tar.gz) to read metadata from"))
                .arg(Arg::new("no-wiki").long("no-wiki").action(ArgAction::SetTrue))
                .arg(Arg::new("no-lfs").long("no-lfs").action(ArgAction::SetTrue)),
        )
        .subcommand(
            Command::new("sync")
                .about("Keep the import current: one polling pass, or serve webhooks and poll")
                .arg(repo.clone())
                .args(common.clone())
                .arg(Arg::new("webhook-listen").long("webhook-listen").help("address for POST /webhooks/github"))
                .arg(Arg::new("webhook-secret").long("webhook-secret").help("the GitHub webhook secret"))
                .arg(Arg::new("poll-secs").long("poll-secs").help("polling interval while serving (default 300)")),
        )
        .subcommand(
            Command::new("cutover")
                .about("Freeze, final sync, redirect map and final report")
                .arg(repo.clone())
                .args(common.clone())
                .arg(Arg::new("archive-github").long("archive-github").action(ArgAction::SetTrue).help("archive the GitHub repository first")),
        )
        .subcommand(
            Command::new("claim-token")
                .about("Print the claim token for a GitHub account's placeholder")
                .arg(repo)
                .arg(Arg::new("github-id").required(true))
                .args(common),
        )
}

fn s<'a>(m: &'a ArgMatches, k: &str) -> Option<&'a str> {
    m.try_get_one::<String>(k)
        .ok()
        .flatten()
        .map(String::as_str)
}

fn flag(m: &ArgMatches, k: &str) -> bool {
    m.try_get_one::<bool>(k)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

fn credential(m: &ArgMatches) -> Result<Credential, BeeError> {
    if let Some(t) = s(m, "github-token") {
        return Ok(Credential::Token(t.to_string()));
    }
    if let (Some(id), Some(key), Some(inst)) =
        (s(m, "app-id"), s(m, "app-key"), s(m, "installation"))
    {
        let pem = std::fs::read_to_string(key)
            .map_err(|e| bee_github::error::io("reading the App key", e))?;
        let inst: u64 = inst.parse().map_err(|_| {
            bee_github::error::invalid("invalid_installation", "--installation must be a number")
        })?;
        return Ok(Credential::App(AppCredential::new(id, &pem, inst)?));
    }
    let (t, _) = discover_token("github.com")?;
    Ok(Credential::Token(t))
}

struct Setup {
    importer: Importer,
    server: Option<(String, Option<String>)>,
    token: Option<String>,
}

fn setup(m: &ArgMatches, dry: bool) -> Result<Setup, BeeError> {
    let repo = s(m, "repo").unwrap_or_default().to_string();
    if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
        return Err(bee_github::error::invalid(
            "invalid_repo",
            "the repository must be owner/name",
        ));
    }
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cfg = s(m, "api-url").map(ClientConfig::at).unwrap_or_default();
    let cred = credential(m)?;
    let token = match &cred {
        Credential::Token(t) => Some(t.clone()),
        _ => None,
    };
    let client = Client::new(cfg, cred, clock.clone());
    let state_dir = s(m, "state")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".bee-github").join(&repo));
    let state = StateDir::open(state_dir)?;
    let mut opts = Options::new(repo.clone());
    if let Some(h) = s(m, "hub-url") {
        opts.hub_url = h.to_string();
    }
    opts.hub_org = s(m, "org").map(str::to_string);
    opts.mirror_only = flag(m, "mirror-only");
    opts.wiki = !flag(m, "no-wiki");
    opts.lfs = !flag(m, "no-lfs");
    opts.archive = s(m, "archive").map(PathBuf::from);
    opts.at_ms = clock.now_secs() * 1000;
    let endpoint = s(m, "server")
        .map(str::to_string)
        .or_else(|| std::env::var("BEE_ENDPOINT").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "http://127.0.0.1:7420".into());
    let hub_token = s(m, "token")
        .map(str::to_string)
        .or_else(|| std::env::var("BEE_TOKEN").ok().filter(|v| !v.is_empty()));
    let org = opts
        .hub_org
        .clone()
        .unwrap_or_else(|| repo.split('/').next().unwrap_or("").to_string());
    let name = repo.split('/').nth(1).unwrap_or("").to_string();
    let git = Arc::new(ServerTarget::new(
        rpc(&endpoint, hub_token.clone()),
        &org,
        &name,
        clock.clone(),
    ));
    // Until the server hosts forge imports, metadata is journaled and reported as not yet
    // placed; a dry run only estimates.
    let sink: Arc<dyn bee_github::ForgeSink> = if dry {
        Arc::new(MemorySink::new())
    } else {
        Arc::new(UnwiredSink)
    };
    Ok(Setup {
        importer: Importer::new(client, state, git, sink, opts),
        server: Some((endpoint, hub_token)),
        token,
    })
}

async fn run(m: &ArgMatches, sub: &str) -> Result<(Value, String), BeeError> {
    match sub {
        "import" => {
            let dry = flag(m, "dry-run");
            let st = setup(m, dry)?;
            if dry {
                let e = st.importer.dry_run().await?;
                let human = format!(
                    "{}: {} PRs, {} issues, ~{} requests, ~{} min at the current rate limit ({} of {} left)",
                    e.repository,
                    e.pull_requests,
                    e.issues,
                    e.requests,
                    e.eta_secs / 60,
                    e.rate_remaining,
                    e.rate_limit
                );
                return Ok((json!(e), human));
            }
            // A server that hosts forge imports runs the whole thing itself.
            if let (Some((endpoint, hub)), Some(tok)) = (&st.server, &st.token) {
                let opts = st.importer.options();
                let org = opts
                    .hub_org
                    .clone()
                    .unwrap_or_else(|| opts.repo.split('/').next().unwrap_or("").into());
                if let Some(id) =
                    start_github_import(&rpc(endpoint, hub.clone()), &opts.repo, &org, tok).await?
                {
                    return Ok((
                        json!({ "import_id": id, "server": endpoint }),
                        format!("the server is importing {} (import {id})", opts.repo),
                    ));
                }
            }
            let r = st.importer.run().await?;
            let human = r.report.human();
            Ok((
                json!({ "phase": r.phase.as_str(), "report": r.report.to_value(), "redirects": r.redirects.len(), "state": st.importer.state().root() }),
                human,
            ))
        }
        "sync" => {
            let st = setup(m, false)?;
            let importer = Arc::new(st.importer);
            let first = importer.sync().await?;
            let Some(addr) = s(m, "webhook-listen") else {
                return Ok((
                    json!(first),
                    format!("synced; {} records placed", first.placed.len()),
                ));
            };
            let secret = s(m, "webhook-secret").ok_or_else(|| {
                bee_github::error::invalid(
                    "missing_secret",
                    "--webhook-secret is required with --webhook-listen",
                )
            })?;
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| bee_github::error::io("binding the webhook listener", e))?;
            let app = webhook_router(WebhookApp {
                importer: importer.clone(),
                secret: Arc::new(secret.as_bytes().to_vec()),
            });
            let poll = s(m, "poll-secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(300u64);
            let poller = importer.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(poll)).await;
                    if let Err(e) = poller.sync().await {
                        eprintln!(
                            "bee-github: poll failed: {} {}",
                            e.reason.as_str(),
                            e.message
                        );
                        if e.reason == bee_core::Reason::InvalidState {
                            return;
                        }
                    }
                }
            });
            eprintln!("bee-github: serving POST http://{addr}/webhooks/github");
            axum::serve(listener, app)
                .await
                .map_err(|e| bee_github::error::unavailable(format!("webhook server: {e}")))?;
            Ok((json!({}), String::new()))
        }
        "cutover" => {
            let st = setup(m, false)?;
            let r = st.importer.cutover(flag(m, "archive-github")).await?;
            let human = r.report.human();
            Ok((
                json!({ "phase": r.phase.as_str(), "report": r.report.to_value(), "redirects": r.redirects }),
                human,
            ))
        }
        "claim-token" => {
            let st = setup(m, false)?;
            let id: u64 = s(m, "github-id")
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| {
                    bee_github::error::invalid(
                        "invalid_user",
                        "the GitHub user id must be a number",
                    )
                })?;
            let t = st.importer.claim_token(id)?;
            Ok((
                json!({ "github_id": id, "principal": bee_github::principals::principal_id(id), "claim_token": t }),
                t,
            ))
        }
        other => Err(bee_github::error::invalid(
            "unknown_command",
            format!("unknown command {other}"),
        )),
    }
}

fn main() {
    let matches = cli().get_matches();
    let (sub, m) = matches.subcommand().expect("a subcommand is required");
    let json_out = flag(m, "json");
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("bee-github: cannot start the runtime: {e}");
            std::process::exit(2);
        }
    };
    match rt.block_on(run(m, sub)) {
        Ok((v, human)) => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else if !human.is_empty() {
                println!("{human}");
            }
        }
        Err(e) => {
            if json_out {
                println!(
                    "{}",
                    json!({ "error": { "reason": e.reason.as_str(), "message": e.message } })
                );
            } else {
                eprintln!("bee-github: {}: {}", e.reason.as_str(), e.message);
            }
            std::process::exit(1);
        }
    }
}
