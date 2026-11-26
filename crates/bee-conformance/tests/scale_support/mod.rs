//! Shared harness for the W14 scale tests (`linearizability`, `complexity`, `many_agents`,
//! `faults`): an in-process server (SQLite + disk, or Postgres when `BEE_TEST_PG` is set),
//! the gate/full size switch, recorded seeds and a logical clock for histories.

#![allow(dead_code, missing_docs, clippy::result_large_err)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bee_conformance::load::client::{self, Client};
use bee_core::{ObjectId, RefName};
use bee_proto::v1;
use bee_server::config::Config;
use bee_server::server::{RunningServer, ServerOptions, start};
use bee_store::FaultInjector;

/// True when `BEE_SCALE_FULL=1`: run the large sizes instead of the gate sizes.
pub fn full() -> bool {
    std::env::var("BEE_SCALE_FULL").is_ok_and(|v| v == "1")
}

/// `gate` normally, `full` under `BEE_SCALE_FULL=1`.
pub fn size(gate: usize, full_size: usize) -> usize {
    if full() { full_size } else { gate }
}

/// The seed of a randomized test: `BEE_SCALE_SEED` when set, else `default`. Printed so a
/// failing run names the seed that reproduces its choices.
pub fn seed(test: &str, default: u64) -> u64 {
    let s = std::env::var("BEE_SCALE_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    eprintln!("{test}: seed {s} (set BEE_SCALE_SEED to replay)");
    s
}

/// A logical clock: every call returns a larger number, in real-time order across tasks.
#[derive(Debug, Default)]
pub struct Ticks(AtomicU64);

impl Ticks {
    pub fn tick(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

/// A config rooted at `dir`, on Postgres when `BEE_TEST_PG` is set.
pub fn config(dir: &std::path::Path) -> Config {
    let mut cfg = Config::dev(dir.to_path_buf(), Some("127.0.0.1:0".into()));
    cfg.redaction.purge_interval_secs = 3600;
    cfg.forge.relay_poll_ms = 50;
    cfg.forge.webhook_poll_ms = 50;
    cfg.forge.bridge_interval_ms = 50;
    cfg.limits.default_deadline_ms = 120_000;
    cfg.logging.level = "error".into();
    if let Ok(url) = std::env::var("BEE_TEST_PG") {
        cfg.metadata.backend = "postgres".into();
        cfg.metadata.postgres_url = url;
        cfg.metadata.postgres_schema = format!("w14_{}", bee_server::util::random_hex(6));
    }
    cfg
}

/// A running server with its data directory and a client holding the dev root token.
pub struct Srv {
    pub srv: RunningServer,
    pub client: Client,
    pub cfg: Config,
}

impl Srv {
    pub fn repo(&self) -> bee_store::RepoId {
        self.srv.dev.as_ref().expect("dev bootstrap").repo
    }

    pub fn user(&self) -> String {
        self.srv.dev.as_ref().expect("dev bootstrap").user.clone()
    }

    /// A fresh connection (agents use their own channels).
    pub async fn channel(&self) -> tonic::transport::Channel {
        client::connect(&self.srv.url).await.expect("connect")
    }

    /// The fsck verdict of the dev org.
    pub async fn fsck_clean(&self) -> (bool, Vec<String>) {
        let r = v1::admin_service_client::AdminServiceClient::new(self.client.channel().clone())
            .run_fsck(self.client.req(v1::RunFsckRequest {
                org: "dev".into(),
                ..Default::default()
            }))
            .await
            .expect("fsck")
            .into_inner();
        (
            r.clean,
            r.violations
                .iter()
                .map(|v| format!("{}: {}", v.kind, v.detail))
                .collect(),
        )
    }

    /// Asserts the ref log of `name` is gap-free from 1 to its tip, each entry starting where
    /// the previous ended, and that the landed index holds exactly the entries' changes.
    pub async fn assert_ref_log_continuous(&self, name: &str) -> u64 {
        let r = RefName::new(name).expect("ref name");
        let meta = &self.srv.state.meta;
        let log = meta
            .ref_log(self.repo(), &r, 1, 10_000_000)
            .await
            .expect("ref log");
        let versions: Vec<u64> = log.iter().map(|e| e.version).collect();
        let last = versions.last().copied().unwrap_or(0);
        assert_eq!(
            versions,
            (1..=last).collect::<Vec<_>>(),
            "{name}: ref log has gaps"
        );
        for w in log.windows(2) {
            assert_eq!(w[0].new, w[1].old, "{name}: entries chain");
        }
        let landed = meta
            .list_landed(self.repo(), Some(&r))
            .await
            .expect("landed");
        let total: usize = log.iter().map(|e| e.landed.len()).sum();
        assert_eq!(
            landed.len(),
            total,
            "{name}: every landed change indexed once"
        );
        last
    }
}

/// Starts a server on `cfg`; `dir` keeps the data directory alive (several servers may share
/// one `cfg` and so one store).
pub async fn start_on(cfg: Config, faults: Option<Arc<FaultInjector>>) -> Srv {
    start_opts(cfg, |o| o.faults = faults).await
}

/// Starts a server on `cfg` after `f` adjusted its options (forge store, faults).
pub async fn start_opts(cfg: Config, f: impl FnOnce(&mut ServerOptions)) -> Srv {
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = true;
    f(&mut opts);
    let srv = start(opts).await.expect("server starts");
    let token = srv.dev.as_ref().expect("dev bootstrap").token.clone();
    let ch = client::connect(&srv.url).await.expect("connect");
    Srv {
        client: Client::new(ch, token, "dev/playground"),
        srv,
        cfg,
    }
}

/// Starts one server in a fresh directory.
pub async fn start_fresh(
    f: impl FnOnce(&mut Config),
    faults: Option<Arc<FaultInjector>>,
) -> (Srv, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());
    f(&mut cfg);
    (start_on(cfg, faults).await, dir)
}

/// The tip of `main`.
pub async fn main_tip(c: &Client) -> (ObjectId, u64) {
    c.resolve("main")
        .await
        .expect("resolve")
        .expect("main exists")
}
