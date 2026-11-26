//! Prometheus metrics with fixed names (PRODUCT §4.22), rendered as text at `/metrics`.
//!
//! | Name | Type | Labels |
//! |---|---|---|
//! | `bee_up` | gauge | |
//! | `bee_rpc_requests_total` | counter | `method`, `code` |
//! | `bee_rpc_duration_seconds` | histogram | `method` |
//! | `bee_inflight_requests` | gauge | `tier` |
//! | `bee_admission_shed_total` | counter | `tier`, `why` |
//! | `bee_land_batches_total` | counter | |
//! | `bee_land_batch_size` | histogram | |
//! | `bee_lands_total` | counter | `result` |
//! | `bee_land_cas_retries_total` | counter | |
//! | `bee_coalescer_owner_changes_total` | counter | |
//! | `bee_events_appended_total` | counter | |
//! | `bee_audit_entries_total` | counter | |
//! | `bee_secret_findings_total` | counter | `outcome` |
//! | `bee_integrity_failures_total` | counter | `source` |
//! | `bee_git_*` | | the git gateway's, `bee_git_http::metrics` |

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const BUCKETS: [f64; 14] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];
const SIZE_BUCKETS: [f64; 10] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0];

#[derive(Default, Clone)]
struct Histogram {
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    fn observe(&mut self, buckets: &[f64], v: f64) {
        if self.counts.len() != buckets.len() {
            self.counts = vec![0; buckets.len()];
        }
        for (i, b) in buckets.iter().enumerate() {
            if v <= *b {
                self.counts[i] += 1;
            }
        }
        self.sum += v;
        self.count += 1;
    }

    fn render(&self, out: &mut String, name: &str, labels: &str, buckets: &[f64]) {
        let sep = if labels.is_empty() { "" } else { "," };
        for (i, b) in buckets.iter().enumerate() {
            let c = self.counts.get(i).copied().unwrap_or(0);
            out.push_str(&format!("{name}_bucket{{{labels}{sep}le=\"{b}\"}} {c}\n"));
        }
        out.push_str(&format!(
            "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {}\n",
            self.count
        ));
        let l = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        out.push_str(&format!(
            "{name}_sum{l} {}\n{name}_count{l} {}\n",
            self.sum, self.count
        ));
    }
}

/// The server's metrics registry.
#[derive(Default)]
pub struct Metrics {
    requests: Mutex<BTreeMap<(String, String), u64>>,
    durations: Mutex<BTreeMap<String, Histogram>>,
    inflight: Mutex<BTreeMap<String, i64>>,
    shed: Mutex<BTreeMap<(String, String), u64>>,
    lands: Mutex<BTreeMap<String, u64>>,
    batch_sizes: Mutex<Histogram>,
    /// Coalesced CAS publications.
    pub land_batches: AtomicU64,
    /// CAS retries inside coalescers.
    pub land_cas_retries: AtomicU64,
    /// Coalescer ownership acquisitions.
    pub owner_changes: AtomicU64,
    /// Events written.
    pub events: AtomicU64,
    /// Audit entries written.
    pub audit_entries: AtomicU64,
    secret_findings: Mutex<BTreeMap<String, u64>>,
    integrity: Mutex<BTreeMap<String, u64>>,
    last_integrity: Mutex<Option<IntegrityFailure>>,
    /// The git gateway's counters (`bee_git_*`), shared by its HTTP routes and SSH
    /// listener and rendered with the rest.
    pub git: std::sync::Arc<bee_git_http::metrics::GitMetrics>,
}

/// The latest integrity failure (`crate::integrity`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityFailure {
    /// When it was seen.
    pub at_ms: u64,
    /// Where: `blob_store` or `fsck`.
    pub source: String,
    /// The object id.
    pub object: String,
    /// What failed.
    pub detail: String,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Metrics {
    /// A fresh registry.
    pub fn new() -> Metrics {
        Metrics::default()
    }

    /// Records a finished RPC.
    pub fn rpc(&self, method: &str, code: &str, secs: f64) {
        *lock(&self.requests)
            .entry((method.to_string(), code.to_string()))
            .or_default() += 1;
        lock(&self.durations)
            .entry(method.to_string())
            .or_default()
            .observe(&BUCKETS, secs);
    }

    /// Adjusts the in-flight gauge of a tier.
    pub fn inflight(&self, tier: &str, delta: i64) {
        *lock(&self.inflight).entry(tier.to_string()).or_default() += delta;
    }

    /// Records an integrity failure: counts it, keeps it as the latest, and writes an `error`
    /// log line from the `integrity` component.
    pub fn integrity_failure(&self, source: &str, object: &str, detail: &str, at_ms: u64) {
        *lock(&self.integrity).entry(source.to_string()).or_default() += 1;
        *lock(&self.last_integrity) = Some(IntegrityFailure {
            at_ms,
            source: source.to_string(),
            object: object.to_string(),
            detail: detail.to_string(),
        });
        crate::logging::log(
            crate::logging::Level::Error,
            "integrity",
            "integrity check failed",
            &[
                ("source", source.into()),
                ("object", object.into()),
                ("detail", detail.into()),
            ],
        );
    }

    /// Integrity failures seen, every source.
    pub fn integrity_failures(&self) -> u64 {
        lock(&self.integrity).values().sum()
    }

    /// The latest integrity failure.
    pub fn last_integrity_failure(&self) -> Option<IntegrityFailure> {
        lock(&self.last_integrity).clone()
    }

    /// Records a shed request.
    pub fn shed(&self, tier: &str, why: &str) {
        *lock(&self.shed)
            .entry((tier.to_string(), why.to_string()))
            .or_default() += 1;
    }

    /// Records a land outcome (`landed`, `skipped`, `conflict`, `denied`, `error`).
    pub fn land(&self, result: &str) {
        *lock(&self.lands).entry(result.to_string()).or_default() += 1;
    }

    /// Records one coalesced publication of `size` stacks.
    pub fn batch(&self, size: usize) {
        self.land_batches.fetch_add(1, Ordering::Relaxed);
        lock(&self.batch_sizes).observe(&SIZE_BUCKETS, size as f64);
    }

    /// Records secret-scanning findings.
    pub fn secret_findings(&self, outcome: &str, n: u64) {
        *lock(&self.secret_findings)
            .entry(outcome.to_string())
            .or_default() += n;
    }

    /// Count of lands with a given result.
    pub fn lands(&self, result: &str) -> u64 {
        lock(&self.lands).get(result).copied().unwrap_or(0)
    }

    /// Prometheus text exposition.
    pub fn render(&self) -> String {
        let mut o = String::new();
        o.push_str("# TYPE bee_up gauge\nbee_up 1\n");
        o.push_str("# TYPE bee_rpc_requests_total counter\n");
        for ((m, c), n) in lock(&self.requests).iter() {
            o.push_str(&format!(
                "bee_rpc_requests_total{{method=\"{m}\",code=\"{c}\"}} {n}\n"
            ));
        }
        o.push_str("# TYPE bee_rpc_duration_seconds histogram\n");
        for (m, h) in lock(&self.durations).iter() {
            h.render(
                &mut o,
                "bee_rpc_duration_seconds",
                &format!("method=\"{m}\""),
                &BUCKETS,
            );
        }
        o.push_str("# TYPE bee_inflight_requests gauge\n");
        for (t, n) in lock(&self.inflight).iter() {
            o.push_str(&format!("bee_inflight_requests{{tier=\"{t}\"}} {n}\n"));
        }
        o.push_str("# TYPE bee_admission_shed_total counter\n");
        for ((t, w), n) in lock(&self.shed).iter() {
            o.push_str(&format!(
                "bee_admission_shed_total{{tier=\"{t}\",why=\"{w}\"}} {n}\n"
            ));
        }
        o.push_str("# TYPE bee_land_batches_total counter\n");
        o.push_str(&format!(
            "bee_land_batches_total {}\n",
            self.land_batches.load(Ordering::Relaxed)
        ));
        o.push_str("# TYPE bee_land_batch_size histogram\n");
        lock(&self.batch_sizes).render(&mut o, "bee_land_batch_size", "", &SIZE_BUCKETS);
        o.push_str("# TYPE bee_lands_total counter\n");
        for (r, n) in lock(&self.lands).iter() {
            o.push_str(&format!("bee_lands_total{{result=\"{r}\"}} {n}\n"));
        }
        for (name, v) in [
            ("bee_land_cas_retries_total", &self.land_cas_retries),
            ("bee_coalescer_owner_changes_total", &self.owner_changes),
            ("bee_events_appended_total", &self.events),
            ("bee_audit_entries_total", &self.audit_entries),
        ] {
            o.push_str(&format!(
                "# TYPE {name} counter\n{name} {}\n",
                v.load(Ordering::Relaxed)
            ));
        }
        o.push_str("# TYPE bee_integrity_failures_total counter\n");
        for (s, n) in lock(&self.integrity).iter() {
            o.push_str(&format!(
                "bee_integrity_failures_total{{source=\"{s}\"}} {n}\n"
            ));
        }
        o.push_str("# TYPE bee_secret_findings_total counter\n");
        for (r, n) in lock(&self.secret_findings).iter() {
            o.push_str(&format!(
                "bee_secret_findings_total{{outcome=\"{r}\"}} {n}\n"
            ));
        }
        o.push_str(&self.git.render());
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_fixed_names() {
        let m = Metrics::new();
        m.rpc("/bee.v1.WriteService/Land", "OK", 0.004);
        m.batch(3);
        m.land("landed");
        m.shed("anonymous", "concurrency");
        let text = m.render();
        assert!(text.contains(
            "bee_rpc_requests_total{method=\"/bee.v1.WriteService/Land\",code=\"OK\"} 1"
        ));
        assert!(text.contains(
            "bee_rpc_duration_seconds_bucket{method=\"/bee.v1.WriteService/Land\",le=\"0.005\"} 1"
        ));
        assert!(text.contains("bee_land_batch_size_bucket{le=\"4\"} 1"));
        assert!(text.contains("bee_lands_total{result=\"landed\"} 1"));
        assert!(
            text.contains("bee_admission_shed_total{tier=\"anonymous\",why=\"concurrency\"} 1")
        );
        assert_eq!(m.lands("landed"), 1);
    }
}
