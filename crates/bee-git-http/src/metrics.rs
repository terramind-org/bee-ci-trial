//! The gateway's Prometheus metrics (`docs/spec/git-gateway.md` §10), rendered by the server
//! at `/metrics` next to its own:
//!
//! | Name | Type | Labels |
//! |---|---|---|
//! | `bee_git_requests_total` | counter | `transport` (`http`, `ssh`), `service`, `outcome` (`ok`, `error`) |
//! | `bee_git_request_duration_seconds` | histogram | `transport`, `service` |
//! | `bee_git_fetch_protocol_total` | counter | `transport`, `version` (`0`, `1`, `2`) |
//! | `bee_git_upload_pack_bytes_total` | counter | `transport` |
//! | `bee_git_receive_pack_bytes_total` | counter | `transport` |
//! | `bee_git_pack_cache_total` | counter | `result` (`hit`, `miss`) |
//! | `bee_git_push_ref_updates_total` | counter | `result` (`ok`, `rejected`) |
//! | `bee_git_lfs_bytes_total` | counter | `direction` (`upload`, `download`) |
//!
//! `service` is one of `info-refs`, `upload-pack`, `receive-pack`, `lfs-batch`, `lfs-transfer`,
//! `lfs-locks` and `lfs-authenticate`.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Upper bounds of the duration histogram, in seconds.
const BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

#[derive(Clone, Default)]
struct Histogram {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

/// The gateway's counters, shared by its HTTP routes and SSH listener.
#[derive(Default)]
pub struct GitMetrics {
    requests: Mutex<BTreeMap<(String, String, String), u64>>,
    durations: Mutex<BTreeMap<(String, String), Histogram>>,
    protocols: Mutex<BTreeMap<(String, String), u64>>,
    upload_bytes: Mutex<BTreeMap<String, u64>>,
    receive_bytes: Mutex<BTreeMap<String, u64>>,
    pack_cache: Mutex<BTreeMap<String, u64>>,
    ref_updates: Mutex<BTreeMap<String, u64>>,
    lfs_bytes: Mutex<BTreeMap<String, u64>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn add<K: Ord>(m: &Mutex<BTreeMap<K, u64>>, k: K, n: u64) {
    *lock(m).entry(k).or_default() += n;
}

impl GitMetrics {
    /// A fresh registry.
    pub fn new() -> GitMetrics {
        GitMetrics::default()
    }

    /// A finished request of `service` over `transport`.
    pub fn request(&self, transport: &str, service: &str, ok: bool, secs: f64) {
        let outcome = if ok { "ok" } else { "error" };
        add(
            &self.requests,
            (transport.into(), service.into(), outcome.into()),
            1,
        );
        let mut d = lock(&self.durations);
        let h = d.entry((transport.into(), service.into())).or_default();
        if h.counts.len() != BUCKETS.len() {
            h.counts = vec![0; BUCKETS.len()];
        }
        for (i, b) in BUCKETS.iter().enumerate() {
            if secs <= *b {
                h.counts[i] += 1;
            }
        }
        h.sum += secs;
        h.count += 1;
    }

    /// A fetch (upload-pack request) in protocol `version`.
    pub fn fetch_protocol(&self, transport: &str, version: u8) {
        add(&self.protocols, (transport.into(), version.to_string()), 1);
    }

    /// Bytes an upload-pack response sent.
    pub fn upload_bytes(&self, transport: &str, n: u64) {
        add(&self.upload_bytes, transport.into(), n);
    }

    /// Bytes a push sent to receive-pack.
    pub fn receive_bytes(&self, transport: &str, n: u64) {
        add(&self.receive_bytes, transport.into(), n);
    }

    /// A pack cache outcome (`hit` or `miss`; `none` is not counted).
    pub fn pack_cache(&self, outcome: &str) {
        if outcome == "hit" || outcome == "miss" {
            add(&self.pack_cache, outcome.into(), 1);
        }
    }

    /// Ref updates of a push: `ok` written, `rejected` refused.
    pub fn ref_updates(&self, ok: u64, rejected: u64) {
        if ok > 0 {
            add(&self.ref_updates, "ok".into(), ok);
        }
        if rejected > 0 {
            add(&self.ref_updates, "rejected".into(), rejected);
        }
    }

    /// LFS object bytes moved (`upload` or `download`).
    pub fn lfs_bytes(&self, direction: &str, n: u64) {
        add(&self.lfs_bytes, direction.into(), n);
    }

    /// The Prometheus text exposition of every gateway metric.
    pub fn render(&self) -> String {
        let mut o = String::new();
        o.push_str("# TYPE bee_git_requests_total counter\n");
        for ((t, s, r), n) in lock(&self.requests).iter() {
            o.push_str(&format!(
                "bee_git_requests_total{{transport=\"{t}\",service=\"{s}\",outcome=\"{r}\"}} {n}\n"
            ));
        }
        o.push_str("# TYPE bee_git_request_duration_seconds histogram\n");
        for ((t, s), h) in lock(&self.durations).iter() {
            let l = format!("transport=\"{t}\",service=\"{s}\"");
            for (i, b) in BUCKETS.iter().enumerate() {
                o.push_str(&format!(
                    "bee_git_request_duration_seconds_bucket{{{l},le=\"{b}\"}} {}\n",
                    h.counts.get(i).copied().unwrap_or(0)
                ));
            }
            o.push_str(&format!(
                "bee_git_request_duration_seconds_bucket{{{l},le=\"+Inf\"}} {}\n\
                 bee_git_request_duration_seconds_sum{{{l}}} {}\n\
                 bee_git_request_duration_seconds_count{{{l}}} {}\n",
                h.count, h.sum, h.count
            ));
        }
        o.push_str("# TYPE bee_git_fetch_protocol_total counter\n");
        for ((t, v), n) in lock(&self.protocols).iter() {
            o.push_str(&format!(
                "bee_git_fetch_protocol_total{{transport=\"{t}\",version=\"{v}\"}} {n}\n"
            ));
        }
        for (name, m) in [
            ("bee_git_upload_pack_bytes_total", &self.upload_bytes),
            ("bee_git_receive_pack_bytes_total", &self.receive_bytes),
        ] {
            o.push_str(&format!("# TYPE {name} counter\n"));
            for (t, n) in lock(m).iter() {
                o.push_str(&format!("{name}{{transport=\"{t}\"}} {n}\n"));
            }
        }
        for (name, label, m) in [
            ("bee_git_pack_cache_total", "result", &self.pack_cache),
            (
                "bee_git_push_ref_updates_total",
                "result",
                &self.ref_updates,
            ),
            ("bee_git_lfs_bytes_total", "direction", &self.lfs_bytes),
        ] {
            o.push_str(&format!("# TYPE {name} counter\n"));
            for (k, n) in lock(m).iter() {
                o.push_str(&format!("{name}{{{label}=\"{k}\"}} {n}\n"));
            }
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_metric_with_its_labels() {
        let m = GitMetrics::new();
        m.request("http", "upload-pack", true, 0.02);
        m.request("ssh", "receive-pack", false, 3.0);
        m.fetch_protocol("http", 2);
        m.upload_bytes("http", 100);
        m.receive_bytes("ssh", 50);
        m.pack_cache("hit");
        m.pack_cache("none");
        m.ref_updates(2, 1);
        m.lfs_bytes("download", 7);
        let t = m.render();
        for line in [
            "bee_git_requests_total{transport=\"http\",service=\"upload-pack\",outcome=\"ok\"} 1",
            "bee_git_requests_total{transport=\"ssh\",service=\"receive-pack\",outcome=\"error\"} 1",
            "bee_git_request_duration_seconds_bucket{transport=\"http\",service=\"upload-pack\",le=\"0.025\"} 1",
            "bee_git_request_duration_seconds_bucket{transport=\"ssh\",service=\"receive-pack\",le=\"2.5\"} 0",
            "bee_git_request_duration_seconds_count{transport=\"ssh\",service=\"receive-pack\"} 1",
            "bee_git_fetch_protocol_total{transport=\"http\",version=\"2\"} 1",
            "bee_git_upload_pack_bytes_total{transport=\"http\"} 100",
            "bee_git_receive_pack_bytes_total{transport=\"ssh\"} 50",
            "bee_git_pack_cache_total{result=\"hit\"} 1",
            "bee_git_push_ref_updates_total{result=\"ok\"} 2",
            "bee_git_push_ref_updates_total{result=\"rejected\"} 1",
            "bee_git_lfs_bytes_total{direction=\"download\"} 7",
        ] {
            assert!(t.contains(line), "{line}\n{t}");
        }
        assert!(!t.contains("result=\"none\""));
    }
}
