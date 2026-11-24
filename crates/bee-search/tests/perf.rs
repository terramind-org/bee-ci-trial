//! Small performance check: index 50k generated source files and measure query latency.
//! Prints the numbers (run with `--nocapture`); the assertion bound is loose because debug
//! builds and shared CI machines vary widely.

use crate::common;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bee_core::Ulid;
use bee_search::MemoryIndexStore;
use bee_search::search::{CodeSearchRequest, Searcher};
use bee_search::store::IndexStore;
use bee_store::{OrgId, RepoId};
use common::allow_all;

const FILES: usize = 50_000;
const REPOS: usize = 10;

/// A deterministic generator (64-bit LCG); no randomness source is read.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() as usize) % xs.len()]
    }
}

const WORDS: &[&str] = &[
    "user", "account", "invoice", "total", "amount", "request", "response", "handler", "config",
    "cache", "token", "session", "order", "item", "price", "client", "server", "buffer", "parse",
    "render", "update", "delete", "create", "fetch", "store", "index", "query", "result", "error",
];

fn source(rng: &mut Lcg, i: usize) -> String {
    let mut s = String::with_capacity(1200);
    s.push_str(&format!(
        "// generated file {i}\nuse crate::{}::{};\n\n",
        rng.pick(WORDS),
        rng.pick(WORDS)
    ));
    for f in 0..6 {
        let a = rng.pick(WORDS);
        let b = rng.pick(WORDS);
        s.push_str(&format!(
            "pub fn {a}_{b}_{n}(input: &str) -> Result<u64, Error> {{\n    let {b} = {a}::parse(input)?;\n    if {b}.len() > {m} {{\n        return Err(Error::new(\"{a} too long\"));\n    }}\n    Ok({b}.len() as u64)\n}}\n\n",
            n = (i * 7 + f) % 9973,
            m = rng.next() % 1000,
        ));
    }
    s
}

#[test]
fn index_50k_files_and_query_latency() {
    let store = Arc::new(MemoryIndexStore::new());
    let org = OrgId(Ulid::from_u128(1));
    let mut rng = Lcg(42);
    let t0 = Instant::now();
    let mut bytes = 0usize;
    let mut targets = Vec::new();
    for r in 0..REPOS {
        let mut files = BTreeMap::new();
        for i in 0..FILES / REPOS {
            let n = r * (FILES / REPOS) + i;
            let dir = ["src", "src/api", "src/core/model", "lib", "tests"][n % 5];
            let content = source(&mut rng, n);
            bytes += content.len();
            files.insert(
                format!("{dir}/m{}/file_{n}.rs", n % 97),
                content.into_bytes(),
            );
        }
        let mut t = common::index_files(
            store.as_ref(),
            org,
            RepoId(Ulid::from_u128(100 + r as u128)),
            &files,
        );
        t.repo_name = format!("repo{r}");
        targets.push(t);
    }
    let index_time = t0.elapsed();
    assert_eq!(store.doc_count(org).unwrap(), FILES as u64);

    let searcher = Searcher::new(store.clone() as Arc<dyn IndexStore>);
    let queries = [
        "invoice_total_17",
        "\"generated file 4242\"",
        "/fn account_[a-z]+_99\\b/",
        "cache_token_5 path:src/api/",
        "/Error::new\\(\"render too long\"\\)/ repo:repo3",
        "session_order_1234",
        "\"file 49999\"",
        "price_client_42 OR buffer_parse_42",
        "symbol:render_update_7",
        "/server_[a-z]+_77\\(/ lang:rust",
    ];
    let mut latencies: Vec<Duration> = Vec::new();
    let mut hits_seen = 0u64;
    for round in 0..5 {
        for q in queries {
            let t = Instant::now();
            let r = searcher
                .search_code(
                    &CodeSearchRequest {
                        query: q.into(),
                        ..CodeSearchRequest::default()
                    },
                    &targets,
                    &allow_all,
                )
                .unwrap();
            latencies.push(t.elapsed());
            if round == 0 {
                hits_seen += r.total_count;
            }
        }
    }
    latencies.sort();
    let p = |q: f64| latencies[((latencies.len() as f64 - 1.0) * q).round() as usize];
    eprintln!(
        "perf: indexed {FILES} files ({:.1} MB) in {:.2?}; {} queries: p50 {:.2?}, p95 {:.2?}, max {:.2?}; {} hits in round 0",
        bytes as f64 / 1e6,
        index_time,
        latencies.len(),
        p(0.5),
        p(0.95),
        latencies[latencies.len() - 1],
        hits_seen
    );
    assert!(hits_seen > 0);
    assert!(p(0.95) < Duration::from_secs(5), "p95 {:?}", p(0.95));
}
