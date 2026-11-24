//! `bee-server-bench`: measures the SPEC §16 targets against an in-process server on this
//! machine (SQLite + disk in a temporary directory, gRPC over loopback).
//!
//! ```text
//! cargo run --release -p bee-server --bin bee-server-bench -- [--landers 1000] [--samples 200]
//! ```
//!
//! Reports one-file CreateChangeset latency, uncontended land latency, hot-ref land throughput
//! and time-to-land with N concurrent landers, and log / blame / diff latency on a 10,000-line
//! file with history. Latencies are client-observed (they include loopback gRPC).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bee_proto::v1::{
    self, query_service_client::QueryServiceClient, write_service_client::WriteServiceClient,
};
use bee_server::config::Config;
use bee_server::server::{ServerOptions, start};
use tonic::transport::Channel;

struct Ctx {
    ch: Channel,
    token: String,
}

fn repo() -> Option<v1::RepoRef> {
    Some(v1::RepoRef {
        repo: Some(v1::repo_ref::Repo::FullName("dev/playground".into())),
    })
}

fn pid(id: &bee_core::ObjectId) -> v1::ObjectId {
    v1::ObjectId {
        raw: id.as_bytes().to_vec(),
    }
}

fn cid(n: u64) -> Option<v1::ChangeId> {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&n.to_be_bytes());
    b[15] = 1;
    Some(v1::ChangeId { raw: b.to_vec() })
}

impl Ctx {
    fn req<T>(&self, m: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(m);
        if let Ok(v) = format!("Bearer {}", self.token).parse() {
            r.metadata_mut().insert("authorization", v);
        }
        r
    }

    async fn upload(&self, blobs: &[Vec<u8>]) -> Result<(), tonic::Status> {
        let items: Vec<v1::UploadItem> = blobs
            .iter()
            .enumerate()
            .map(|(i, b)| v1::UploadItem {
                repo: if i == 0 { repo() } else { None },
                id: Some(pid(&bee_object::blob_id(b))),
                offset: 0,
                data: b.clone(),
                last: true,
            })
            .collect();
        WriteServiceClient::new(self.ch.clone())
            .upload_objects(self.req(futures_util::stream::iter(items)))
            .await?;
        Ok(())
    }

    async fn create(
        &self,
        parent: &bee_core::ObjectId,
        path: &str,
        data: &[u8],
        n: u64,
    ) -> Result<bee_core::ObjectId, tonic::Status> {
        let blob = bee_object::blob_id(data);
        let cs = WriteServiceClient::new(self.ch.clone())
            .create_changeset(self.req(v1::CreateChangesetRequest {
                repo: repo(),
                parents: vec![pid(parent)],
                delta: vec![v1::DeltaEntry {
                    path: path.as_bytes().to_vec(),
                    op: Some(v1::delta_entry::Op::Put(v1::Put {
                        mode: 1,
                        blob: Some(pid(&blob)),
                        size: data.len() as u64,
                    })),
                }],
                change_id: cid(n),
                message: format!("change {n}"),
                author_time: Some(v1::Signature {
                    name: "bench".into(),
                    time: Some(pbjson_types::Timestamp {
                        seconds: 1_790_000_000,
                        nanos: 0,
                    }),
                    tz_offset_minutes: 0,
                }),
                ..Default::default()
            }))
            .await?
            .into_inner()
            .changeset
            .and_then(|c| c.id)
            .ok_or_else(|| tonic::Status::internal("no id"))?;
        bee_core::ObjectId::from_bytes(&cs.raw).map_err(|e| tonic::Status::internal(e.to_string()))
    }

    async fn land(
        &self,
        head: &bee_core::ObjectId,
    ) -> Result<(bee_core::ObjectId, u64), tonic::Status> {
        let r = WriteServiceClient::new(self.ch.clone())
            .land(self.req(v1::LandRequest {
                repo: repo(),
                r#ref: "main".into(),
                head: Some(pid(head)),
                ..Default::default()
            }))
            .await?
            .into_inner();
        let rf = r.r#ref.ok_or_else(|| tonic::Status::internal("no ref"))?;
        let target = bee_core::ObjectId::from_bytes(&rf.target.unwrap_or_default().raw)
            .map_err(|e| tonic::Status::internal(e.to_string()))?;
        Ok((target, rf.version))
    }
}

fn pct(v: &mut [Duration], p: f64) -> Duration {
    v.sort();
    let i = ((v.len() as f64 * p).ceil() as usize).clamp(1, v.len()) - 1;
    v[i]
}

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

fn arg(name: &str, default: usize) -> usize {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let landers = arg("--landers", 1000);
    let samples = arg("--samples", 200);
    let dir = std::env::temp_dir().join(format!("bee-bench-{}", bee_server::util::random_hex(6)));
    let mut cfg = Config::dev(dir.clone(), Some("127.0.0.1:0".into()));
    cfg.limits.default_deadline_ms = 300_000;
    // `--pg` runs the metadata store on Postgres at `BEE_BENCH_PG` (or `BEE_TEST_PG`).
    let pg = std::env::var("BEE_BENCH_PG")
        .or_else(|_| std::env::var("BEE_TEST_PG"))
        .ok()
        .filter(|_| std::env::args().any(|a| a == "--pg"));
    if let Some(url) = pg {
        cfg.metadata.backend = "postgres".into();
        cfg.metadata.postgres_url = url;
        cfg.metadata.postgres_schema = format!("bench_{}", bee_server::util::random_hex(4));
    }
    let backend = cfg.metadata.backend.clone();
    let mut opts = ServerOptions::new(cfg);
    opts.dev = true;
    let srv = start(opts).await.map_err(|e| e.detail.message)?;
    let token = srv
        .dev
        .as_ref()
        .map(|d| d.token.clone())
        .unwrap_or_default();
    let ch = tonic::transport::Endpoint::from_shared(srv.url.clone())?
        .connect()
        .await?;
    let ctx = Arc::new(Ctx { ch, token });
    let empty = bee_object::Changeset::empty_root().id();
    let mut n = 0u64;

    // 1. One-file CreateChangeset (blob uploaded beforehand, as a client does).
    let blobs: Vec<Vec<u8>> = (0..samples)
        .map(|i| format!("create {i}\n").into_bytes())
        .collect();
    ctx.upload(&blobs).await?;
    let mut create = Vec::new();
    for b in &blobs {
        n += 1;
        let t0 = Instant::now();
        ctx.create(&empty, &format!("c/{n}.txt"), b, n).await?;
        create.push(t0.elapsed());
    }

    // 2. Uncontended land of a one-change stack.
    let mut tip = empty;
    let mut land = Vec::new();
    let blobs: Vec<Vec<u8>> = (0..samples)
        .map(|i| format!("land {i}\n").into_bytes())
        .collect();
    ctx.upload(&blobs).await?;
    for b in &blobs {
        n += 1;
        let head = ctx.create(&tip, &format!("u/{n}.txt"), b, n).await?;
        let t0 = Instant::now();
        let (t, _) = ctx.land(&head).await?;
        land.push(t0.elapsed());
        tip = t;
    }

    // 3. Hot ref: N concurrent landers, each with its own change on the same tip.
    let blobs: Vec<Vec<u8>> = (0..landers)
        .map(|i| format!("hot {i}\n").into_bytes())
        .collect();
    ctx.upload(&blobs).await?;
    let mut heads = Vec::new();
    for b in &blobs {
        n += 1;
        heads.push(ctx.create(&tip, &format!("h/{n}.txt"), b, n).await?);
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(landers));
    let wall = Instant::now();
    // Landers spread over 16 connections, like independent agents.
    let mut lander_ctx = Vec::new();
    for _ in 0..16 {
        let ch = tonic::transport::Endpoint::from_shared(srv.url.clone())?
            .connect()
            .await?;
        lander_ctx.push(Arc::new(Ctx {
            ch,
            token: ctx.token.clone(),
        }));
    }
    let mut handles = Vec::new();
    for (i, h) in heads.into_iter().enumerate() {
        let ctx = lander_ctx[i % lander_ctx.len()].clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let t0 = Instant::now();
            ctx.land(&h).await.map(|r| (r, t0.elapsed()))
        }));
    }
    let mut hot = Vec::new();
    let mut max_version = 0;
    for h in handles {
        let ((t, v), d) = h.await??;
        if v > max_version {
            max_version = v;
            tip = t;
        }
        hot.push(d);
    }
    let hot_wall = wall.elapsed();
    let batches = srv
        .state
        .metrics
        .land_batches
        .load(std::sync::atomic::Ordering::Relaxed);

    // 4. History: a 10,000-line file edited 50 times, then log / blame / diff.
    let mut lines: Vec<String> = (0..10_000)
        .map(|i| format!("line {i} of the big file\n"))
        .collect();
    for e in 0..50 {
        lines[e * 197 % 10_000] = format!("edited {e}\n");
        let data = lines.concat().into_bytes();
        ctx.upload(std::slice::from_ref(&data)).await?;
        n += 1;
        let head = ctx.create(&tip, "big.txt", &data, n).await?;
        tip = ctx.land(&head).await?.0;
    }
    let mut qc = QueryServiceClient::new(ctx.ch.clone());
    let (mut log, mut blame, mut diff) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..samples.min(100) {
        let t0 = Instant::now();
        qc.log(ctx.req(v1::LogRequest {
            repo: repo(),
            from: Some(v1::Revision {
                expr: "main".into(),
            }),
            include_meta: true,
            page: Some(v1::PageRequest {
                page_size: 100,
                page_token: String::new(),
            }),
            ..Default::default()
        }))
        .await?;
        log.push(t0.elapsed());
        let t0 = Instant::now();
        qc.blame(ctx.req(v1::BlameRequest {
            repo: repo(),
            at: Some(v1::Revision {
                expr: "main".into(),
            }),
            path: b"big.txt".to_vec(),
            ..Default::default()
        }))
        .await?;
        blame.push(t0.elapsed());
        let t0 = Instant::now();
        let mut s = qc
            .diff(ctx.req(v1::DiffRequest {
                repo: repo(),
                from: Some(v1::Revision {
                    expr: "main~1".into(),
                }),
                to: Some(v1::Revision {
                    expr: "main".into(),
                }),
                include_hunks: true,
                ..Default::default()
            }))
            .await?
            .into_inner();
        while s.message().await?.is_some() {}
        diff.push(t0.elapsed());
    }

    println!("Metadata store: {backend}; blobs: disk; {landers} landers; {samples} samples");
    println!();
    println!("| Operation | Target | p50 | p99 | Other |");
    println!("|---|---|---|---|---|");
    println!(
        "| CreateChangeset, 1 file ({samples} samples) | p50 ≤ 10 ms, p99 ≤ 50 ms | {} | {} | |",
        ms(pct(&mut create, 0.5)),
        ms(pct(&mut create, 0.99))
    );
    println!(
        "| Land, uncontended, 1-change stack ({samples} samples) | p50 ≤ 30 ms, p99 ≤ 150 ms | {} | {} | |",
        ms(pct(&mut land, 0.5)),
        ms(pct(&mut land, 0.99))
    );
    println!(
        "| Land, one hot ref, {landers} concurrent landers | ≥ 100 lands/s, p99 time-to-land ≤ 10 s | {} | {} | {:.0} lands/s; {} CAS batches; wall {:.2} s |",
        ms(pct(&mut hot, 0.5)),
        ms(pct(&mut hot, 0.99)),
        landers as f64 / hot_wall.as_secs_f64(),
        batches,
        hot_wall.as_secs_f64()
    );
    println!(
        "| Log, 100-entry page | p99 ≤ 200 ms | {} | {} | |",
        ms(pct(&mut log, 0.5)),
        ms(pct(&mut log, 0.99))
    );
    println!(
        "| Blame, 10,000-line file, 50 edits | p99 ≤ 200 ms | {} | {} | |",
        ms(pct(&mut blame, 0.5)),
        ms(pct(&mut blame, 0.99))
    );
    println!(
        "| Diff, one 10,000-line file with hunks | p99 ≤ 200 ms | {} | {} | |",
        ms(pct(&mut diff, 0.5)),
        ms(pct(&mut diff, 0.99))
    );
    srv.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
