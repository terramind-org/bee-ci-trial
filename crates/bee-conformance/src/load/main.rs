//! `load`: the SPEC §16 load harness (plan task W14).
//!
//! Starts an in-process `bee-server` (SQLite and disk blobs in a temporary directory, or
//! Postgres with `--pg` and `BEE_BENCH_PG` / `BEE_TEST_PG`), drives it over gRPC on
//! loopback the way agents do, and prints a Markdown report judging every SPEC §16 target
//! (`bee_conformance::load::report`).
//!
//! ```text
//! cargo build --release -p bee-conformance --example load
//! target/release/examples/load [--landers 1000] [--samples 200] [--sessions 10000]
//!     [--files 10000] [--pg] [--out report.md]
//! ```
//!
//! Workloads, in order: one-file CreateChangeset in a small repo; uncontended land;
//! `--sessions` event subscriptions held open while lands are delivered to all of them; the
//! repo grows by `--files` files and CreateChangeset is measured again; `--landers`
//! concurrent landers on one protected ref (the coalescer), then the delivery time of one
//! event right after that burst; history reads (log, blame, diff on a 10,000-line file with
//! 50 edits) alone and under 32 concurrent mixed readers; sparse checkout of 1,000 files
//! through the working-copy library, cold against the server and warm through a node daemon;
//! code-hosting clone throughput over smart HTTP; code search latency. Latencies are
//! client-observed and include loopback gRPC.

#[path = "support.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bee_conformance::load::client::{self, Client, change_id, ref_updated};
use bee_conformance::load::report::{self, Measured, Measurement, Report, RunInfo};
use bee_conformance::load::stats::{Summary, ms, summarize};
use bee_core::ObjectId;
use bee_proto::v1;
use bee_server::config::Config;
use bee_server::server::{ServerOptions, start};
use support::{
    BoxError, arg, arg_str, checkout, detail, dir_bytes, git, machine, pct_encode, timed, today,
};

/// Success and failure counts of reads and writes over the whole run.
#[derive(Default)]
struct Outcomes {
    reads: AtomicU64,
    read_failures: AtomicU64,
    writes: AtomicU64,
    write_failures: AtomicU64,
}

impl Outcomes {
    fn read<T, E>(&self, r: &Result<T, E>) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if r.is_err() {
            self.read_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn write<T, E>(&self, r: &Result<T, E>) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        if r.is_err() {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn ratios(&self) -> Measured {
        let ratio = |ok: u64, fail: u64| {
            if ok == 0 {
                1.0
            } else {
                (ok - fail.min(ok)) as f64 / ok as f64
            }
        };
        let r = self.reads.load(Ordering::Relaxed);
        let w = self.writes.load(Ordering::Relaxed);
        Measured::Ratios {
            reads: ratio(r, self.read_failures.load(Ordering::Relaxed)),
            writes: ratio(w, self.write_failures.load(Ordering::Relaxed)),
            requests: r + w,
        }
    }
}

/// Names the call that failed, so a run that stops says where it stopped.
fn at<E: std::fmt::Debug>(what: &'static str) -> impl FnOnce(E) -> BoxError {
    move |e| format!("{what}: {e:?}").into()
}

/// The sequence number of the newest event of the repo (0 when there is none).
async fn newest_event(c: &Client) -> Result<u64, BoxError> {
    let mut newest = 0;
    let mut cursor = 0;
    loop {
        let (events, next) = c.list_events(&[], cursor).await?;
        newest = events.iter().map(|e| e.seq).max().unwrap_or(newest);
        if events.is_empty() || next <= cursor {
            return Ok(newest);
        }
        cursor = next;
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BoxError> {
    let landers = arg("--landers", 1000);
    let samples = arg("--samples", 200);
    let sessions = arg("--sessions", 10_000);
    let files = arg("--files", 10_000);
    let load_before = report::load_average();
    let tmp = tempfile::tempdir()?;
    let mut cfg = Config::dev(tmp.path().join("server"), Some("127.0.0.1:0".into()));
    cfg.limits.default_deadline_ms = 300_000;
    cfg.logging.level = std::env::var("BEE_LOAD_LOG_LEVEL").unwrap_or_else(|_| "error".into());
    let pg = std::env::var("BEE_BENCH_PG")
        .or_else(|_| std::env::var("BEE_TEST_PG"))
        .ok()
        .filter(|_| std::env::args().any(|a| a == "--pg"));
    let store = if let Some(url) = pg {
        cfg.metadata.backend = "postgres".into();
        cfg.metadata.postgres_url = url;
        cfg.metadata.postgres_schema = format!("load_{}", bee_server::util::random_hex(4));
        // `--forge-sqlite` keeps the forge's tables in SQLite while the VCS metadata is on
        // Postgres (the forge's Postgres pool opens a connection per read transaction).
        let forge = if std::env::args().any(|a| a == "--forge-sqlite") {
            cfg.forge.database = format!("sqlite:{}", tmp.path().join("forge").display());
            "forge tables in SQLite"
        } else {
            "forge tables in the same Postgres"
        };
        format!(
            "Postgres 17 (local primary + streaming standby, fsync = on, synchronous_commit = on); {forge}; blobs on local disk"
        )
    } else {
        "SQLite (WAL, synchronous = FULL, F_FULLFSYNC on macOS); blobs on local disk".to_string()
    };
    let mut opts = ServerOptions::new(cfg);
    opts.dev = true;
    let srv = start(opts).await.map_err(|e| e.detail.message)?;
    let token = srv
        .dev
        .as_ref()
        .map(|d| d.token.clone())
        .unwrap_or_default();
    let url = srv.url.clone();
    let ch = client::connect(&url).await?;
    let c = Client::new(ch, token.clone(), "dev/playground");
    let outcomes = Arc::new(Outcomes::default());
    let mut rows = Vec::new();
    let mut extra: Vec<(String, String)> = Vec::new();
    let mut notes = Vec::new();
    let mut n = 0u64;
    let mut next = || {
        n += 1;
        change_id(n)
    };

    // 1. One-file CreateChangeset (blob uploaded first, as a client does), small repo.
    eprintln!("load: phase 1");
    let (mut tip, _) = c.resolve("main").await?.ok_or("no main")?;
    let mut create_small = Vec::new();
    for i in 0..samples {
        let data = format!("create {i}\n").into_bytes();
        c.upload(std::slice::from_ref(&data)).await?;
        let put = [(
            format!("c/{i}.txt"),
            bee_object::blob_id(&data),
            data.len() as u64,
        )];
        let (r, d) = timed(c.create(&tip, &put, next(), "create")).await;
        outcomes.write(&r);
        r?;
        create_small.push(d);
    }

    // 2. Uncontended land of a one-change stack.
    eprintln!("load: phase 2");
    let mut land = Vec::new();
    for i in 0..samples {
        let head = c
            .commit(
                &tip,
                &[(format!("u/{i}.txt"), format!("land {i}\n").into_bytes())],
                next(),
                "land",
            )
            .await?;
        let (r, d) = timed(c.land("main", &head)).await;
        outcomes.write(&r);
        tip = r?.target;
        land.push(d);
    }

    // 3. Sessions and event delivery, before the repo grows and before the hot-ref burst:
    // `sessions` subscriptions held open by 100 agents,
    // then lands; each delivery is timed from the land request to the subscriber.
    eprintln!("load: phase 3");
    let mut agent_tokens = Vec::new();
    for i in 0..100 {
        let id = c
            .create_agent(&format!("load-agent-{i}"))
            .await
            .map_err(at("phase 3: create the agent"))?;
        // Repos are served to org members.
        c.add_member("dev", &id)
            .await
            .map_err(at("phase 3: add the agent to the org"))?;
        agent_tokens.push(
            c.mint_token(&id, &[], 0, 0)
                .await
                .map_err(at("phase 3: mint the agent's token"))?,
        );
    }
    let mut sub_conns = Vec::new();
    for _ in 0..(sessions / 100).max(1) {
        sub_conns.push(
            client::connect(&url)
                .await
                .map_err(at("phase 3: open a subscriber connection"))?,
        );
    }
    // Subscribe from the newest event in the log: nothing older is replayed, and that one
    // event arrives at once (a stream answers its headers with its first message).
    let from = newest_event(&c)
        .await
        .map_err(at("phase 3: read the newest event"))?;
    let first_error: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let lands_to_deliver = 5usize;
    let sent: Arc<std::sync::Mutex<Vec<(u64, Instant)>>> = Arc::default();
    let opened = Arc::new(AtomicU64::new(0));
    let failed_open = Arc::new(AtomicU64::new(0));
    let ready = Arc::new(tokio::sync::Barrier::new(sessions + 1));
    let mut subs = Vec::new();
    for i in 0..sessions {
        let cc = Client::new(
            sub_conns[i % sub_conns.len()].clone(),
            agent_tokens[i % agent_tokens.len()].clone(),
            "dev/playground",
        );
        let (sent, opened, failed_open, ready, first_error) = (
            sent.clone(),
            opened.clone(),
            failed_open.clone(),
            ready.clone(),
            first_error.clone(),
        );
        subs.push(tokio::spawn(async move {
            let stream = tokio::time::timeout(
                Duration::from_secs(120),
                cc.subscribe(&["ref.updated"], from),
            )
            .await
            .unwrap_or_else(|_| Err(tonic::Status::deadline_exceeded("subscribe").into()));
            let mut stream = match stream {
                Ok(s) => {
                    opened.fetch_add(1, Ordering::Relaxed);
                    Some(s)
                }
                Err(e) => {
                    if let Ok(mut f) = first_error.lock() {
                        f.get_or_insert_with(|| e.to_string());
                    }
                    failed_open.fetch_add(1, Ordering::Relaxed);
                    None
                }
            };
            ready.wait().await;
            let mut lat = Vec::new();
            let Some(s) = stream.as_mut() else { return lat };
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            while lat.len() < lands_to_deliver {
                match tokio::time::timeout_at(deadline, s.message()).await {
                    Ok(Ok(Some(e))) => {
                        let at = Instant::now();
                        if let Some((name, _, v)) = ref_updated(&e) {
                            let t0 = sent
                                .lock()
                                .ok()
                                .and_then(|g| g.iter().find(|(x, _)| *x == v).map(|x| x.1));
                            if let (true, Some(t0)) = (name == "main", t0) {
                                lat.push(at.saturating_duration_since(t0));
                            }
                        }
                    }
                    _ => break,
                }
            }
            lat
        }));
    }
    ready.wait().await;
    let open_count = opened.load(Ordering::Relaxed);
    for i in 0..lands_to_deliver {
        let head = c
            .commit(
                &tip,
                &[(format!("ev/{i}.txt"), format!("event {i}\n").into_bytes())],
                next(),
                "event",
            )
            .await
            .map_err(at("phase 3: commit before a land"))?;
        // Record the version this land will produce before sending, so a fast delivery
        // finds its start time.
        let (_, v) = c
            .resolve("main")
            .await
            .map_err(at("phase 3: resolve main before a land"))?
            .ok_or("no main")?;
        let t0 = Instant::now();
        if let Ok(mut g) = sent.lock() {
            g.push((v + 1, t0));
        }
        let r = c.land("main", &head).await;
        outcomes.write(&r);
        tip = r
            .map_err(at("phase 3: land with the subscribers open"))?
            .target;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let mut deliveries = Vec::new();
    let mut complete = 0u64;
    for h in subs {
        let l = h.await?;
        if l.len() == lands_to_deliver {
            complete += 1;
        }
        deliveries.extend(l);
    }
    let delivery = summarize(&deliveries);
    let failed = failed_open.load(Ordering::Relaxed);
    rows.push(Measurement {
        key: "event_delivery",
        measured: Measured::Count {
            count: open_count,
            latency: Some(delivery),
        },
        note: format!(
            "{} deliveries of {lands_to_deliver} lands to {open_count} subscribers; {complete} subscribers got all",
            delivery.count
        ),
    });
    rows.push(Measurement {
        key: "sessions",
        measured: Measured::Count {
            count: complete,
            latency: None,
        },
        note: format!(
            "{sessions} subscriptions attempted over {} connections by 100 agent tokens; {open_count} opened, {failed} refused{}; counted: those that received every event",
            sub_conns.len(),
            first_error
                .lock()
                .ok()
                .and_then(|f| f.clone())
                .map_or_else(String::new, |e| format!(" (first refusal: {e})"))
        ),
    });
    drop(sub_conns);

    // Grow the repo by `files` files (plus a 1,000-file sparse prefix), then CreateChangeset
    // again: the target is independent of repo size.
    let bulk: Vec<(String, Vec<u8>)> = (0..files)
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
        .collect();
    for chunk in bulk.chunks(2000) {
        let blobs: Vec<Vec<u8>> = chunk.iter().map(|f| f.1.clone()).collect();
        c.upload(&blobs).await?;
    }
    let puts: Vec<(String, ObjectId, u64)> = bulk
        .iter()
        .map(|(p, d)| (p.clone(), bee_object::blob_id(d), d.len() as u64))
        .collect();
    let grown = c.create(&tip, &puts, next(), "bulk").await?;
    tip = c.land("main", &grown).await?.target;
    let mut create_large = Vec::new();
    for i in 0..samples {
        let data = format!("create large {i}\n").into_bytes();
        c.upload(std::slice::from_ref(&data)).await?;
        let put = [(
            format!("c2/{i}.txt"),
            bee_object::blob_id(&data),
            data.len() as u64,
        )];
        let (r, d) = timed(c.create(&tip, &put, next(), "create")).await;
        outcomes.write(&r);
        r?;
        create_large.push(d);
    }
    let small = summarize(&create_small);
    rows.push(Measurement {
        key: "create_changeset",
        measured: Measured::Latency(summarize(&create_large)),
        note: format!(
            "repo of {} files; in a {}-file repo: p50 {}, p99 {}",
            files + 1000 + 2 * samples,
            2 * samples,
            ms(small.p50),
            ms(small.p99)
        ),
    });
    rows.push(Measurement {
        key: "land_uncontended",
        measured: Measured::Latency(summarize(&land)),
        note: String::new(),
    });

    // 4. Hot ref: `landers` concurrent landers, each with its own change on the same tip,
    // spread over 16 connections like independent agents.
    eprintln!("load: phase 4");
    let mut heads = Vec::new();
    for i in 0..landers {
        heads.push(
            c.commit(
                &tip,
                &[(format!("h/{i}.txt"), format!("hot {i}\n").into_bytes())],
                next(),
                "hot",
            )
            .await?,
        );
    }
    let mut conns = Vec::new();
    for _ in 0..16 {
        let ch = client::connect(&url).await?;
        conns.push(Client::new(ch, token.clone(), "dev/playground"));
    }
    let batches0 = srv.state.metrics.land_batches.load(Ordering::Relaxed);
    let barrier = Arc::new(tokio::sync::Barrier::new(landers));
    let wall = Instant::now();
    let mut hs = Vec::new();
    for (i, h) in heads.into_iter().enumerate() {
        let cc = conns[i % conns.len()].clone();
        let (barrier, outcomes) = (barrier.clone(), outcomes.clone());
        hs.push(tokio::spawn(async move {
            barrier.wait().await;
            let (r, d) = timed(cc.land("main", &h)).await;
            outcomes.write(&r);
            r.map(|l| (l, d))
        }));
    }
    let mut hot = Vec::new();
    let mut max_v = 0;
    for h in hs {
        let (l, d) = h.await??;
        if l.version > max_v {
            max_v = l.version;
            tip = l.target;
        }
        hot.push(d);
    }
    let hot_wall = wall.elapsed();
    let batches = srv.state.metrics.land_batches.load(Ordering::Relaxed) - batches0;
    rows.push(Measurement {
        key: "land_hot_ref",
        measured: Measured::Throughput {
            per_second: landers as f64 / hot_wall.as_secs_f64(),
            latency: summarize(&hot),
        },
        note: format!(
            "{batches} CAS batches (avg {:.1} lands per batch); wall {:.2} s; max time-to-land {}",
            landers as f64 / batches.max(1) as f64,
            hot_wall.as_secs_f64(),
            ms(summarize(&hot).max)
        ),
    });

    // The forge event log after the burst: the bridge copies every VCS event into the forge
    // log (where subscribers read it). Poll the log until it holds the burst's last land and
    // one more land made right after it.
    {
        let burst_last = max_v;
        let head = c
            .commit(
                &tip,
                &[("ev/backlog.txt".into(), b"backlog\n".to_vec())],
                next(),
                "backlog",
            )
            .await?;
        let t0 = Instant::now();
        let l = c.land("main", &head).await?;
        tip = l.target;
        let wait = Duration::from_secs(60 + landers as u64);
        let (mut burst_done, mut delivered, mut newest) = (None, None, 0);
        let mut cursor = 0;
        while t0.elapsed() < wait && delivered.is_none() {
            loop {
                let (events, next) = c.list_events(&["ref.updated"], cursor).await?;
                for e in &events {
                    if let Some((name, _, v)) = ref_updated(e)
                        && name == "main"
                    {
                        newest = newest.max(v);
                    }
                }
                if events.is_empty() || next <= cursor {
                    break;
                }
                cursor = next;
            }
            if newest >= burst_last && burst_done.is_none() {
                burst_done = Some(t0.elapsed());
            }
            if newest >= l.version {
                delivered = Some(t0.elapsed());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let shown =
            |d: Option<Duration>| d.map_or_else(|| format!("not within {} s", wait.as_secs()), ms);
        extra.push((
            format!("Forge event log after the {landers}-lander burst (polled every 200 ms)"),
            format!(
                "burst's last land in the log after {}; a land made right after it after {}; log held main up to version {newest} of {}",
                shown(burst_done),
                shown(delivered),
                l.version
            ),
        ));
    }

    // 5. History: a 10,000-line file edited 50 times, then log / blame / diff.
    eprintln!("load: phase 5");
    let mut lines: Vec<String> = (0..10_000)
        .map(|i| format!("line {i} of the big file\n"))
        .collect();
    for e in 0..50 {
        lines[e * 197 % 10_000] = format!("edited {e}\n");
        let head = c
            .commit(
                &tip,
                &[("big.txt".into(), lines.concat().into_bytes())],
                next(),
                "edit",
            )
            .await?;
        tip = c.land("main", &head).await?.target;
    }
    let (mut log, mut blame, mut diff) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..samples.min(100) {
        let (r, d) = timed(c.log("main", 100)).await;
        outcomes.read(&r);
        r?;
        log.push(d);
        let (r, d) = timed(c.blame("main", "big.txt")).await;
        outcomes.read(&r);
        r?;
        blame.push(d);
        let (r, d) = timed(c.diff("main~1", "main", false)).await;
        outcomes.read(&r);
        r?;
        diff.push(d);
    }
    // The same reads under 32 concurrent mixed readers.
    let mixed_ops = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    let mixed_wall = Instant::now();
    for k in 0..32usize {
        let cc = conns[k % conns.len()].clone();
        let (outcomes, mixed_ops) = (outcomes.clone(), mixed_ops.clone());
        hs.push(tokio::spawn(async move {
            let mut lat: [Vec<Duration>; 3] = Default::default();
            for i in 0..20usize {
                let which = (k + i) % 5;
                let (ok, d) = match which {
                    0 => {
                        let (r, d) = timed(cc.log("main", 100)).await;
                        outcomes.read(&r);
                        (r.is_ok(), d)
                    }
                    1 => {
                        let (r, d) = timed(cc.blame("main", "big.txt")).await;
                        outcomes.read(&r);
                        (r.is_ok(), d)
                    }
                    2 => {
                        let (r, d) = timed(cc.diff("main~1", "main", false)).await;
                        outcomes.read(&r);
                        (r.is_ok(), d)
                    }
                    3 => {
                        let (r, d) = timed(cc.list_tree("main", "sparse/s01")).await;
                        outcomes.read(&r);
                        (r.is_ok(), d)
                    }
                    _ => {
                        let (r, d) = timed(cc.resolve("main")).await;
                        outcomes.read(&r);
                        (r.is_ok(), d)
                    }
                };
                mixed_ops.fetch_add(1, Ordering::Relaxed);
                if ok && which < 3 {
                    lat[which].push(d);
                }
            }
            lat
        }));
    }
    let mut mixed: [Vec<Duration>; 3] = Default::default();
    for h in hs {
        let l = h.await?;
        for (m, v) in mixed.iter_mut().zip(l) {
            m.extend(v);
        }
    }
    let mixed_secs = mixed_wall.elapsed().as_secs_f64();
    let mixed_note = |i: usize| {
        let s = summarize(&mixed[i]);
        format!(
            "under 32 mixed readers: p50 {}, p99 {}",
            ms(s.p50),
            ms(s.p99)
        )
    };
    rows.push(Measurement {
        key: "log",
        measured: Measured::Latency(summarize(&log)),
        note: mixed_note(0),
    });
    rows.push(Measurement {
        key: "blame",
        measured: Measured::Latency(summarize(&blame)),
        note: format!("10,000 lines, 50 edits; {}", mixed_note(1)),
    });
    rows.push(Measurement {
        key: "diff",
        measured: Measured::Latency(summarize(&diff)),
        note: format!("10,000-line file with hunks; {}", mixed_note(2)),
    });
    extra.push((
        "Mixed reads (log, blame, diff, list tree, resolve), 32 concurrent readers".into(),
        format!(
            "{:.0} reads/s",
            mixed_ops.load(Ordering::Relaxed) as f64 / mixed_secs
        ),
    ));

    // 6. Sparse checkout of 1,000 files: cold straight from the server, warm through a node
    // daemon whose cache already holds the objects (filled by a first checkout).
    eprintln!("load: phase 6");
    let cold_dir = tmp.path().join("wc-cold");
    let warm_fill = tmp.path().join("wc-fill");
    let warm_dir = tmp.path().join("wc-warm");
    let daemon_dir = tempfile::Builder::new().prefix("swl").tempdir_in("/tmp")?;
    let tier = bee_daemon::daemon_tier(
        bee_daemon::DiskConfig::new(daemon_dir.path().join("cache")),
        &url,
        bee_daemon::TierConfig::new(bee_daemon::TierKind::Daemon),
    )
    .map_err(detail)?;
    let socket = daemon_dir.path().join("d.sock");
    let running = bee_daemon::server::serve_unix(tier, &socket)
        .await
        .map_err(detail)?;
    let (tk, u) = (token.clone(), url.clone());
    let sparse = || bee_wc::Sparse::from_prefixes(["sparse"]);
    let sock = socket.clone();
    let (cold, warm) =
        tokio::task::spawn_blocking(move || -> Result<(Duration, Duration), BoxError> {
            let cold = checkout(bee_daemon::Endpoint::Http(u), &tk, sparse()?, &cold_dir)?;
            checkout(
                bee_daemon::Endpoint::Unix(sock.clone()),
                &tk,
                sparse()?,
                &warm_fill,
            )?;
            let warm = checkout(bee_daemon::Endpoint::Unix(sock), &tk, sparse()?, &warm_dir)?;
            Ok((cold, warm))
        })
        .await??;
    drop(running);
    let one = |d: Duration| Summary {
        count: 1,
        p50: d,
        p90: d,
        p99: d,
        max: d,
        mean: d,
    };
    rows.push(Measurement {
        key: "checkout_warm",
        measured: Measured::Latency(one(warm)),
        note: "bee-wc clone through a node daemon with a warm disk cache".into(),
    });
    rows.push(Measurement {
        key: "checkout_cold",
        measured: Measured::Latency(one(cold)),
        note: "bee-wc clone straight from the server (loopback), empty working copy".into(),
    });

    // 7. git clone throughput (smart HTTP, protocol v2).
    eprintln!("load: phase 7");
    let home = tmp.path().join("git-home");
    std::fs::create_dir_all(&home)?;
    std::fs::write(home.join(".gitconfig"), "[protocol]\n\tversion = 2\n")?;
    let clone_url = format!(
        "http://git:{}@{}/dev/playground.git",
        pct_encode(&token),
        srv.addr
    );
    let mut clones = Vec::new();
    let mut clone_bytes = 0;
    for i in 0..3 {
        let dest = home.join(format!("clone{i}"));
        let (h, u, d) = (home.clone(), clone_url.clone(), dest.clone());
        let (r, t) = timed(tokio::task::spawn_blocking(move || {
            git(&h, &["clone", "-q", &u, d.to_str().unwrap_or("clone")])
        }))
        .await;
        r??;
        clone_bytes = dir_bytes(&dest.join(".git"));
        clones.push(t);
    }
    let cs = summarize(&clones);
    extra.push((
        format!(
            "git clone over smart HTTP ({} files, {:.1} MiB packed)",
            files + 1000 + 2 * samples + landers + 250,
            clone_bytes as f64 / 1048576.0
        ),
        format!(
            "p50 {} ({:.1} MiB/s, {:.0} files/s)",
            ms(cs.p50),
            clone_bytes as f64 / 1048576.0 / cs.p50.as_secs_f64(),
            (files + 1000) as f64 / cs.p50.as_secs_f64()
        ),
    ));

    // 8. Code search latency once the index has caught up.
    eprintln!("load: phase 8");
    let mut search = v1::search_service_client::SearchServiceClient::new(c.channel().clone());
    let indexed = Instant::now();
    let mut ready_search = false;
    while indexed.elapsed() < Duration::from_secs(300) {
        let r = search
            .search_code(c.req(v1::SearchCodeRequest {
                query: "\"event 4\"".into(),
                ..Default::default()
            }))
            .await;
        if let Ok(r) = r
            && !r.get_ref().hits.is_empty()
        {
            ready_search = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if ready_search {
        let mut lat = Vec::new();
        for i in 0..samples.min(100) {
            let q = match i % 3 {
                0 => format!("\"bulk file {}\"", (i * 37) % files.max(1)),
                1 => "\"edited 7\"".to_string(),
                _ => format!("sparse file {}", i % 1000),
            };
            let (r, d) = timed(search.search_code(c.req(v1::SearchCodeRequest {
                query: q,
                ..Default::default()
            })))
            .await;
            outcomes.read(&r);
            r?;
            lat.push(d);
        }
        let s = summarize(&lat);
        extra.push((
            format!(
                "Code search, literal queries over {} files (no SPEC §16 bound)",
                files + 1000
            ),
            format!(
                "p50 {}, p99 {} ({} queries); index caught up in {:.1} s",
                ms(s.p50),
                ms(s.p99),
                s.count,
                indexed.elapsed().as_secs_f64()
            ),
        ));
    } else {
        extra.push((
            "Code search".into(),
            "the index did not catch up within 300 s".into(),
        ));
    }

    rows.push(Measurement {
        key: "availability",
        measured: outcomes.ratios(),
        note: "every harness request".into(),
    });
    notes.push(format!(
        "Workload: {samples} samples per latency row, {landers} landers, {sessions} subscriptions, repo grown by {files} files"
    ));
    notes.push("One in-process server, gRPC over loopback; latencies are client-observed".into());
    let report = Report {
        info: RunInfo {
            date: today(),
            machine: machine(),
            store,
            profile: if cfg!(debug_assertions) {
                "debug".into()
            } else {
                "release".into()
            },
            load_before,
            load_after: report::load_average(),
            notes,
        },
        rows,
        extra,
    };
    let md = report.markdown();
    println!("{md}");
    if let Some(path) = arg_str("--out") {
        std::fs::write(path, &md)?;
    }
    srv.shutdown().await;
    Ok(())
}
