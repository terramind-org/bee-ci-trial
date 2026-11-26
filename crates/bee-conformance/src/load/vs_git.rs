//! `vs_git`: bee head to head with plain git on one machine (plan task W14).
//!
//! Starts an in-process `bee-server` (SQLite, disk blobs) and a plain git server
//! (`git http-backend` behind a small CGI shim, smart HTTP, protocol v2) side by side, gives
//! both the same content, and measures the same agent workloads against each:
//!
//! 1. **Contention.** N agents each commit one new file on the same stale tip, then race to
//!    get it onto `main`. A git agent pushes; when the push is rejected it pulls with rebase
//!    and pushes again until it lands. Run against the git server with its default config,
//!    against the git server with every component fsynced (`core.fsync=all`,
//!    `core.fsyncMethod=fsync`), against bee's git gateway with the same unmodified git
//!    client, and with bee's own API (upload + CreateChangeset + Land, timed together).
//! 2. **Full clone** of a 12,000-file repo: `git clone` from each server, and a bee
//!    working copy.
//! 3. **Sparse checkout** of a 1,000-file directory: a blobless partial clone plus
//!    `sparse-checkout` from each server, and a bee sparse working copy.
//! 4. **History reads** on a 10,000-line file with 50 edits: log (100 entries), blame and
//!    diff with local git in a full clone, and through the bee API.
//!
//! ```text
//! cargo build --release -p bee-conformance --example vs_git
//! target/release/examples/vs_git [--agents 10,50,100] [--samples 50] [--clones 3]
//!     [--deadline 600] [--out report.md]
//! ```

#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bee_conformance::load::client::{self, Client, change_id};
use bee_conformance::load::report;
use bee_conformance::load::stats::{Summary, ms, summarize};
use bee_core::ObjectId;
use bee_server::config::Config;
use bee_server::server::{ServerOptions, start};
use support::{
    BoxError, arg, arg_list, arg_str, checkout, git, git_in, machine, pct_encode, timed, today,
};

type Files = Vec<(String, Vec<u8>)>;

/// Runs blocking work (git processes, working-copy checkouts) off the async workers, which
/// also serve the in-process bee server.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, BoxError> + Send + 'static,
) -> Result<T, BoxError> {
    tokio::task::spawn_blocking(f).await?
}

fn write_files(dir: &Path, files: &[(String, Vec<u8>)]) -> Result<(), BoxError> {
    for (p, d) in files {
        let path = dir.join(p);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, d)?;
    }
    Ok(())
}

fn count_files(p: &Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if e.file_name() == ".git" || e.file_name() == ".bee" {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => n += count_files(&e.path()),
                Ok(_) => n += 1,
                Err(_) => {}
            }
        }
    }
    n
}

/// Runs `f` over `items` on `threads` OS threads; the first error wins.
fn par_each<T: Sync>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> Result<(), BoxError> + Sync,
) -> Result<(), BoxError> {
    let chunk = items.len().div_ceil(threads.max(1)).max(1);
    std::thread::scope(|s| {
        let hs: Vec<_> = items
            .chunks(chunk)
            .map(|c| s.spawn(|| c.iter().try_for_each(&f)))
            .collect();
        hs.into_iter()
            .try_for_each(|h| h.join().map_err(|_| BoxError::from("thread panicked"))?)
    })
}

// --- A plain git server: `git http-backend` behind a CGI shim -------------------------------

/// Serves every request by running `git http-backend` as CGI over `root`.
async fn git_cgi(
    axum::extract::State(root): axum::extract::State<Arc<PathBuf>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    match run_cgi(&root, req).await {
        Ok(r) => r,
        Err(e) => axum::response::Response::builder()
            .status(500)
            .body(axum::body::Body::from(e.to_string()))
            .unwrap_or_default(),
    }
}

async fn run_cgi(
    root: &Path,
    req: axum::extract::Request,
) -> Result<axum::response::Response, BoxError> {
    use tokio::io::AsyncWriteExt;
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await?;
    let header = |n: &str| {
        parts
            .headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let mut child = tokio::process::Command::new("git")
        .arg("http-backend")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", root)
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("PATH_INFO", parts.uri.path())
        .env("QUERY_STRING", parts.uri.query().unwrap_or_default())
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("GIT_PROTOCOL", header("git-protocol"))
        .env("REMOTE_USER", "agent")
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(match std::env::var_os("VS_GIT_CGI_LOG") {
            Some(p) => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?
                .into(),
            None => std::process::Stdio::null(),
        })
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let feed = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });
    let out = child.wait_with_output().await?;
    let _ = feed.await;
    let raw = out.stdout;
    let (head_len, sep) = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => (i, 4),
        None => (
            raw.windows(2)
                .position(|w| w == b"\n\n")
                .ok_or("CGI output has no header end")?,
            2,
        ),
    };
    let mut resp = axum::response::Response::builder();
    let mut status = 200u16;
    for line in String::from_utf8_lossy(&raw[..head_len]).lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if k.eq_ignore_ascii_case("status") {
            status = v
                .split(' ')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(500);
        } else {
            resp = resp.header(k.trim(), v);
        }
    }
    Ok(resp
        .status(status)
        .body(axum::body::Body::from(raw[head_len + sep..].to_vec()))?)
}

/// Starts the git server over a fresh bare repo `root/playground.git`; returns its URL.
async fn start_git_server(root: PathBuf) -> Result<String, BoxError> {
    let bare = root.join("playground.git");
    let r = root.clone();
    let b = bare.clone();
    blocking(move || {
        std::fs::create_dir_all(&r)?;
        git(
            &r,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                b.to_str().unwrap_or_default(),
            ],
        )?;
        for (k, v) in [
            ("http.receivepack", "true"),
            ("uploadpack.allowFilter", "true"),
            ("uploadpack.allowAnySHA1InWant", "true"),
        ] {
            git_in(&r, &b, &["config", k, v])?;
        }
        Ok(())
    })
    .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = axum::Router::new()
        .fallback(git_cgi)
        .with_state(Arc::new(root))
        // A push of the grown repo is a pack of many megabytes.
        .layer(axum::extract::DefaultBodyLimit::disable());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{addr}/playground.git"))
}

// --- Content shared by both systems ---------------------------------------------------------

fn seed_files() -> Files {
    (0..1000)
        .map(|i| {
            (
                format!("seed/d{:02}/f{i:04}.txt", i % 20),
                format!("seed file {i}\n").into_bytes(),
            )
        })
        .collect()
}

fn bulk_files(files: usize) -> Files {
    (0..files)
        .map(|i| {
            (
                format!("bulk/d{:02}/e{:02}/f{i:06}.txt", i % 100, (i / 100) % 100),
                format!("bulk file {i}\n").into_bytes(),
            )
        })
        .chain((0..1000).map(|i| {
            (
                format!("sparse/s{:02}/f{i:04}.txt", i % 20),
                format!("sparse file {i} {}\n", "x".repeat(200)).into_bytes(),
            )
        }))
        .collect()
}

/// The 10,000-line file after each of its 50 edits.
fn big_file_versions() -> Vec<Vec<u8>> {
    let mut lines: Vec<String> = (0..10_000)
        .map(|i| format!("line {i} of the big file\n"))
        .collect();
    (0..50)
        .map(|e| {
            lines[e * 197 % 10_000] = format!("edited {e}\n");
            lines.concat().into_bytes()
        })
        .collect()
}

// --- Contention ------------------------------------------------------------------------------

/// One contention run: N agents, one system.
struct Race {
    agents: usize,
    system: &'static str,
    landed: usize,
    wall: Duration,
    latency: Summary,
    /// Push or land attempts over all agents (git: pushes, including rejected ones).
    attempts: u64,
}

/// N git agents, each in its own clone with one commit on the same stale tip, race to push
/// to `main`; a rejected agent pulls with rebase and pushes again until `deadline`.
fn git_race(
    home: &Path,
    work: &Path,
    url: &str,
    agents: usize,
    tag: &str,
    deadline: Duration,
) -> Result<(usize, Duration, Vec<Duration>, u64), BoxError> {
    let dirs: Vec<PathBuf> = (0..agents).map(|i| work.join(format!("a{i}"))).collect();
    par_each(&dirs, 16, |d| {
        let name = d.file_name().and_then(|n| n.to_str()).unwrap_or("a");
        git_in(home, work, &["clone", "-q", url, name])?;
        let file = format!("hot/{tag}/{name}.txt");
        write_files(
            d,
            &[(file.clone(), format!("hot {tag} {name}\n").into_bytes())],
        )?;
        git_in(home, d, &["add", &file])?;
        git_in(home, d, &["commit", "-q", "-m", "hot"])?;
        Ok(())
    })?;
    let barrier = Arc::new(std::sync::Barrier::new(agents + 1));
    let attempts = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for d in dirs.clone() {
        let (barrier, attempts, home) = (barrier.clone(), attempts.clone(), home.to_path_buf());
        hs.push(std::thread::spawn(move || {
            barrier.wait();
            let t0 = Instant::now();
            loop {
                attempts.fetch_add(1, Ordering::Relaxed);
                if git_in(&home, &d, &["push", "-q", "origin", "HEAD:main"]).is_ok() {
                    return Some(t0.elapsed());
                }
                if t0.elapsed() > deadline {
                    return None;
                }
                // Rejected (non-fast-forward or a lost ref lock race): rebase onto the new tip.
                let _ = git_in(&home, &d, &["pull", "-q", "--rebase", "origin", "main"]);
            }
        }));
    }
    barrier.wait();
    let wall = Instant::now();
    let mut lat = Vec::new();
    for h in hs {
        if let Some(d) = h.join().map_err(|_| "agent thread panicked")? {
            lat.push(d);
        }
    }
    let wall = wall.elapsed();
    for d in &dirs {
        let _ = std::fs::remove_dir_all(d);
    }
    Ok((lat.len(), wall, lat, attempts.load(Ordering::Relaxed)))
}

/// N agents on bee's own API: each uploads, creates a changeset on the same stale tip and
/// lands it, timed together, over 16 connections.
async fn bee_race(
    url: &str,
    token: &str,
    c: &Client,
    agents: usize,
    tag: &str,
    ids: &AtomicU64,
) -> Result<(usize, Duration, Vec<Duration>, u64), BoxError> {
    let (tip, _) = c.resolve("main").await?.ok_or("no main")?;
    let mut conns = Vec::new();
    for _ in 0..16 {
        conns.push(Client::new(
            client::connect(url).await?,
            token.to_string(),
            "dev/playground",
        ));
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(agents + 1));
    let mut hs = Vec::new();
    for i in 0..agents {
        let cc = conns[i % conns.len()].clone();
        let barrier = barrier.clone();
        let id = change_id(ids.fetch_add(1, Ordering::Relaxed));
        let file = (
            format!("hot/{tag}/t{i}.txt"),
            format!("hot {tag} t{i}\n").into_bytes(),
        );
        hs.push(tokio::spawn(async move {
            barrier.wait().await;
            let t0 = Instant::now();
            let head = cc.commit(&tip, &[file], id, "hot").await.ok()?;
            cc.land("main", &head).await.ok()?;
            Some(t0.elapsed())
        }));
    }
    barrier.wait().await;
    let wall = Instant::now();
    let mut lat = Vec::new();
    for h in hs {
        if let Some(d) = h.await? {
            lat.push(d);
        }
    }
    Ok((lat.len(), wall.elapsed(), lat, agents as u64))
}

// --- Report ----------------------------------------------------------------------------------

fn secs(d: Duration) -> String {
    if d >= Duration::from_secs(1) {
        format!("{:.2} s", d.as_secs_f64())
    } else {
        ms(d)
    }
}

fn three(samples: &[Duration]) -> String {
    let s = summarize(samples);
    format!(
        "{} (min {}, max {})",
        secs(s.p50),
        secs(*samples.iter().min().unwrap_or(&s.p50)),
        secs(s.max)
    )
}

#[allow(clippy::too_many_arguments)]
fn render(
    header: &[(String, String)],
    races: &[Race],
    clones: &[(&str, Vec<Duration>, usize)],
    sparse: &[(&str, Vec<Duration>, usize)],
    reads: &[(&str, Summary, Summary)],
) -> String {
    let mut md = String::from("## bee vs git\n\n");
    for (k, v) in header {
        md.push_str(&format!("- {k}: {v}\n"));
    }
    md.push_str(
        "\n### Contention: N agents land one commit each on `main`\n\n\
         | Agents | System | Landed | Wall | Lands/s | Time to land p50 | p99 | Attempts |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    for r in races {
        md.push_str(&format!(
            "| {} | {} | {}/{} | {} | {:.1} | {} | {} | {} |\n",
            r.agents,
            r.system,
            r.landed,
            r.agents,
            secs(r.wall),
            r.landed as f64 / r.wall.as_secs_f64().max(1e-9),
            secs(r.latency.p50),
            secs(r.latency.p99),
            r.attempts
        ));
    }
    md.push_str(
        "\n### Full clone\n\n| Client and server | p50 (min, max) | Files |\n|---|---|---|\n",
    );
    for (name, t, files) in clones {
        md.push_str(&format!("| {name} | {} | {files} |\n", three(t)));
    }
    md.push_str(
        "\n### Sparse checkout of one 1,000-file directory\n\n| Client and server | p50 (min, max) | Files |\n|---|---|---|\n",
    );
    for (name, t, files) in sparse {
        md.push_str(&format!("| {name} | {} | {files} |\n", three(t)));
    }
    md.push_str(
        "\n### History reads, 10,000-line file with 50 edits\n\n| Read | git, local full clone p50 / p99 | bee API p50 / p99 |\n|---|---|---|\n",
    );
    for (name, g, t) in reads {
        md.push_str(&format!(
            "| {name} | {} / {} | {} / {} |\n",
            ms(g.p50),
            ms(g.p99),
            ms(t.p50),
            ms(t.p99)
        ));
    }
    md
}

// --- The run ---------------------------------------------------------------------------------

fn main() -> Result<(), BoxError> {
    // Up to a few hundred git agent threads plus their blocking helpers.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(1024)
        .build()?
        .block_on(run())
}

async fn run() -> Result<(), BoxError> {
    let agent_counts = arg_list("--agents", &[10, 50, 100]);
    let samples = arg("--samples", 50);
    let clone_runs = arg("--clones", 3);
    let files = arg("--files", 10_000);
    let deadline = Duration::from_secs(arg("--deadline", 600) as u64);
    let load_before = report::load_average();
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_path_buf();

    // Servers.
    let mut cfg = Config::dev(root.join("server"), Some("127.0.0.1:0".into()));
    cfg.limits.default_deadline_ms = 300_000;
    cfg.logging.level = "error".into();
    let mut opts = ServerOptions::new(cfg);
    opts.dev = true;
    let srv = start(opts).await.map_err(|e| e.detail.message)?;
    let token = srv
        .dev
        .as_ref()
        .map(|d| d.token.clone())
        .unwrap_or_default();
    let url = srv.url.clone();
    let c = Client::new(
        client::connect(&url).await?,
        token.clone(),
        "dev/playground",
    );
    let bee_git = format!(
        "http://git:{}@{}/dev/playground.git",
        pct_encode(&token),
        srv.addr
    );
    let git_url = start_git_server(root.join("git-root")).await?;
    let bare = root.join("git-root/playground.git");

    let home = root.join("home");
    std::fs::create_dir_all(&home)?;
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = agent\n\temail = agent@agents.bee.local\n\
         [protocol]\n\tversion = 2\n[init]\n\tdefaultBranch = main\n\
         [advice]\n\tdetachedHead = false\n[gc]\n\tauto = 0\n\
         [http]\n\tpostBuffer = 536870912\n",
    )?;
    let git_version = git_in(&home, &home, &["--version"])?.trim().to_string();
    let ids = AtomicU64::new(1);
    let next = || change_id(ids.fetch_add(1, Ordering::Relaxed));

    // Seed both with the same 1,000 files.
    eprintln!("vs_git: seeding");
    let seed = seed_files();
    let (tip, _) = c.resolve("main").await?.ok_or("no main")?;
    let blobs: Vec<Vec<u8>> = seed.iter().map(|f| f.1.clone()).collect();
    c.upload(&blobs).await?;
    let puts: Vec<(String, ObjectId, u64)> = seed
        .iter()
        .map(|(p, d)| (p.clone(), bee_object::blob_id(d), d.len() as u64))
        .collect();
    let head = c.create(&tip, &puts, next(), "seed").await?;
    c.land("main", &head).await?;
    let author = root.join("author");
    {
        let (h, a, u, s) = (home.clone(), author.clone(), git_url.clone(), seed.clone());
        blocking(move || {
            git(&h, &["clone", "-q", &u, a.to_str().unwrap_or_default()])?;
            write_files(&a, &s)?;
            git_in(&h, &a, &["add", "-A"])?;
            git_in(&h, &a, &["commit", "-q", "-m", "seed"])?;
            git_in(&h, &a, &["push", "-q", "origin", "HEAD:main"])?;
            Ok(())
        })
        .await?;
    }

    // 1. Contention.
    let mut races = Vec::new();
    for &n in &agent_counts {
        for system in [
            "git server, default config",
            "git server, core.fsync=all",
            "bee git gateway, git client",
            "bee API",
        ] {
            eprintln!("vs_git: contention, {n} agents, {system}");
            let tag = format!("n{n}-{}", races.len());
            let (landed, wall, lat, attempts) = if system == "bee API" {
                bee_race(&url, &token, &c, n, &tag, &ids).await?
            } else {
                let durable = system.contains("fsync");
                let (h, b) = (home.clone(), bare.clone());
                let u = if system.starts_with("bee") {
                    bee_git.clone()
                } else {
                    git_url.clone()
                };
                let work = root.join(format!("race-{tag}"));
                blocking(move || {
                    std::fs::create_dir_all(&work)?;
                    if durable {
                        git_in(&h, &b, &["config", "core.fsync", "all"])?;
                        git_in(&h, &b, &["config", "core.fsyncMethod", "fsync"])?;
                    }
                    let r = git_race(&h, &work, &u, n, &tag, deadline);
                    if durable {
                        git_in(&h, &b, &["config", "--unset", "core.fsync"])?;
                        git_in(&h, &b, &["config", "--unset", "core.fsyncMethod"])?;
                    }
                    r
                })
                .await?
            };
            let latency = summarize(&lat);
            // Logged as it lands, so a later failure does not lose the contention results.
            eprintln!(
                "vs_git: {n} agents, {system}: {landed}/{n} landed, wall {}, p50 {}, p99 {}, {attempts} attempts",
                secs(wall),
                secs(latency.p50),
                secs(latency.p99)
            );
            races.push(Race {
                agents: n,
                system,
                landed,
                wall,
                latency,
                attempts,
            });
        }
    }

    // Grow both: `files` bulk files plus a 1,000-file sparse directory, then a 10,000-line
    // file edited 50 times, one commit per edit.
    eprintln!("vs_git: growing both repos");
    let bulk = bulk_files(files);
    let versions = big_file_versions();
    let (mut tip, _) = c.resolve("main").await?.ok_or("no main")?;
    for chunk in bulk.chunks(2000) {
        c.upload(&chunk.iter().map(|f| f.1.clone()).collect::<Vec<_>>())
            .await?;
    }
    let puts: Vec<(String, ObjectId, u64)> = bulk
        .iter()
        .map(|(p, d)| (p.clone(), bee_object::blob_id(d), d.len() as u64))
        .collect();
    let grown = c.create(&tip, &puts, next(), "bulk").await?;
    tip = c.land("main", &grown).await?.target;
    for v in &versions {
        let head = c
            .commit(&tip, &[("big.txt".into(), v.clone())], next(), "edit")
            .await?;
        tip = c.land("main", &head).await?.target;
    }
    {
        let (h, a, b, v) = (home.clone(), author.clone(), bulk.clone(), versions.clone());
        blocking(move || {
            git_in(&h, &a, &["pull", "-q", "--rebase", "origin", "main"])?;
            write_files(&a, &b)?;
            git_in(&h, &a, &["add", "-A"])?;
            git_in(&h, &a, &["commit", "-q", "-m", "bulk"])?;
            for d in v {
                std::fs::write(a.join("big.txt"), d)?;
                git_in(&h, &a, &["add", "big.txt"])?;
                git_in(&h, &a, &["commit", "-q", "-m", "edit"])?;
            }
            git_in(&h, &a, &["push", "-q", "origin", "HEAD:main"])?;
            Ok(())
        })
        .await?;
    }

    // 2. Full clone.
    eprintln!("vs_git: full clone");
    let mut clones: Vec<(&str, Vec<Duration>, usize)> = Vec::new();
    for (name, u) in [
        ("git clone ← git server", git_url.clone()),
        ("git clone ← bee git gateway", bee_git.clone()),
    ] {
        let mut t = Vec::new();
        let mut n_files = 0;
        for i in 0..clone_runs {
            let dest = root.join(format!("clone-{}-{i}", clones.len()));
            let (h, u, d) = (home.clone(), u.clone(), dest.clone());
            let (r, d_t) = timed(blocking(move || {
                git(&h, &["clone", "-q", &u, d.to_str().unwrap_or_default()])
            }))
            .await;
            r?;
            t.push(d_t);
            n_files = count_files(&dest);
            if i > 0 || !clones.is_empty() {
                let _ = std::fs::remove_dir_all(&dest);
            }
        }
        clones.push((name, t, n_files));
    }
    {
        let mut t = Vec::new();
        let mut n_files = 0;
        for i in 0..clone_runs {
            let dest = root.join(format!("wc-full-{i}"));
            let (tk, u, d) = (token.clone(), url.clone(), dest.clone());
            t.push(
                blocking(move || {
                    checkout(
                        bee_daemon::Endpoint::Http(u),
                        &tk,
                        bee_wc::Sparse::full(),
                        &d,
                    )
                })
                .await?,
            );
            n_files = count_files(&dest);
            let _ = std::fs::remove_dir_all(&dest);
        }
        clones.push(("bee working copy ← bee server", t, n_files));
    }

    // 3. Sparse checkout of `sparse/` (1,000 files).
    eprintln!("vs_git: sparse checkout");
    let mut sparse: Vec<(&str, Vec<Duration>, usize)> = Vec::new();
    for (name, u) in [
        (
            "git partial clone + sparse-checkout ← git server",
            git_url.clone(),
        ),
        (
            "git partial clone + sparse-checkout ← bee git gateway",
            bee_git.clone(),
        ),
    ] {
        let mut t = Vec::new();
        let mut n_files = 0;
        for i in 0..clone_runs {
            let dest = root.join(format!("sparse-{}-{i}", sparse.len()));
            let (h, u, d) = (home.clone(), u.clone(), dest.clone());
            let (r, d_t) = timed(blocking(move || {
                git(
                    &h,
                    &[
                        "clone",
                        "-q",
                        "--filter=blob:none",
                        "--no-checkout",
                        &u,
                        d.to_str().unwrap_or_default(),
                    ],
                )?;
                git_in(&h, &d, &["sparse-checkout", "set", "--no-cone", "/sparse/"])?;
                git_in(&h, &d, &["checkout", "-q", "main"])?;
                Ok(())
            }))
            .await;
            r?;
            t.push(d_t);
            n_files = count_files(&dest);
            let _ = std::fs::remove_dir_all(&dest);
        }
        sparse.push((name, t, n_files));
    }
    {
        let mut t = Vec::new();
        let mut n_files = 0;
        for i in 0..clone_runs {
            let dest = root.join(format!("wc-sparse-{i}"));
            let (tk, u, d) = (token.clone(), url.clone(), dest.clone());
            t.push(
                blocking(move || {
                    checkout(
                        bee_daemon::Endpoint::Http(u),
                        &tk,
                        bee_wc::Sparse::from_prefixes(["sparse"])?,
                        &d,
                    )
                })
                .await?,
            );
            n_files = count_files(&dest);
            let _ = std::fs::remove_dir_all(&dest);
        }
        sparse.push(("bee sparse working copy ← bee server", t, n_files));
    }

    // 4. History reads: local git in the first full clone, and the bee API.
    eprintln!("vs_git: history reads");
    let full = root.join("clone-0-0");
    let mut reads = Vec::new();
    for (name, args) in [
        ("Log, 100 entries", vec!["log", "-n", "100"]),
        ("Blame, big.txt", vec!["blame", "big.txt"]),
        ("Diff, last commit", vec!["diff", "HEAD~1", "HEAD"]),
    ] {
        let (h, d) = (home.clone(), full.clone());
        let g = blocking(move || {
            let mut lat = Vec::new();
            for _ in 0..samples {
                let t0 = Instant::now();
                git_in(&h, &d, &args)?;
                lat.push(t0.elapsed());
            }
            Ok(lat)
        })
        .await?;
        let mut t = Vec::new();
        for _ in 0..samples {
            let (r, d) = match name {
                "Log, 100 entries" => {
                    let (r, d) = timed(c.log("main", 100)).await;
                    (r.map(|_| ()), d)
                }
                "Blame, big.txt" => {
                    let (r, d) = timed(c.blame("main", "big.txt")).await;
                    (r.map(|_| ()), d)
                }
                _ => {
                    let (r, d) = timed(c.diff("main~1", "main", false)).await;
                    (r.map(|_| ()), d)
                }
            };
            r?;
            t.push(d);
        }
        reads.push((name, summarize(&g), summarize(&t)));
    }

    let fmt_load = |l: Option<f64>| l.map_or_else(|| "unknown".into(), |l| format!("{l:.2}"));
    let header = vec![
        ("Date".into(), today()),
        ("Machine".into(), machine()),
        ("git".into(), git_version),
        (
            "bee".into(),
            format!(
                "in-process server, SQLite (WAL, synchronous = FULL, F_FULLFSYNC on macOS), {} build",
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                }
            ),
        ),
        (
            "git server".into(),
            "git http-backend per request behind a CGI shim, smart HTTP, protocol v2, loopback; client http.postBuffer = 512 MiB, so pushes carry a length rather than chunked bodies"
                .into(),
        ),
        (
            "Load average (1 min)".into(),
            format!(
                "{} before, {} after",
                fmt_load(load_before),
                fmt_load(report::load_average())
            ),
        ),
        (
            "Repo".into(),
            format!(
                "1,000 seed files during contention; then {files} bulk files, a 1,000-file sparse directory and a 10,000-line file with 50 edits"
            ),
        ),
    ];
    let md = render(&header, &races, &clones, &sparse, &reads);
    println!("{md}");
    if let Some(path) = arg_str("--out") {
        std::fs::write(path, &md)?;
    }
    srv.shutdown().await;
    Ok(())
}
