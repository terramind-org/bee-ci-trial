//! `OpsService` (SPEC §15, `docs/spec/operations.md` §3): the operator surface of one
//! server process (one node of its cell).
//!
//! | area | source |
//! |---|---|
//! | health | live probes of the metadata store, server store, blob store, forge and search index |
//! | cluster | this cell (a single-cell deployment is its own directory) |
//! | metrics | the fixed Prometheus registry, sampled every `ops.metrics_sample_ms` for ranges and rates |
//! | logs | the structured log lines this process wrote (a bounded ring), live tail |
//! | traces | one span per RPC (method, duration, status), keyed by request id |
//! | config | strict validation with key paths; the effective config with secrets redacted |
//! | rate limits | the caller's principal and delegation-root buckets |
//! | rollouts | binary and config changes this cell recorded at startup |
//! | status page | incidents and components in the server store |
//! | backups | `AdminService` backups and restore drills |
//! | cache tiers | none run inside this process (daemons and regional caches report themselves) |
//!
//! Health, rate limits and the status page answer any authenticated caller; everything else
//! is server-wide and needs an operator (every principal of the chain listed in
//! `ops.operators` or `forge.site_admins`); administering an org is not enough.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bee_proto::v1::ops_service_server::OpsService;
use bee_proto::v1::{self, ErrorReason};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::Caller;
use crate::errors::{ApiError, ApiResult};
use crate::logging::{self, Level};
use crate::services::support::{
    get_json, next_page, offset_page, page_of, put_json, require_operator, scan_json,
};
use crate::services::{Rpc, RpcStream};
use crate::state::{AppState, RequestMeta};
use crate::util::timestamp_ms;

/// Incidents (`<id>`).
pub const NS_INCIDENT: &str = "ops_incident";
/// Status components (`<id>`).
pub const NS_COMPONENT: &str = "ops_component";
/// Rollouts (`<id>`).
pub const NS_ROLLOUT: &str = "ops_rollout";
/// The versions this cell last ran (`binary`, `config`).
pub const NS_VERSION: &str = "ops_version";

/// Spans kept for trace queries.
pub const TRACE_CAPACITY: usize = 10_000;
/// Metric samples kept for range queries.
pub const SAMPLE_CAPACITY: usize = 720;

/// One recorded RPC span.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    /// Request id.
    pub request_id: String,
    /// Trace id (32 hex digits; from `traceparent`, or new).
    pub trace_id: String,
    /// This span (16 hex digits).
    pub span_id: String,
    /// The caller's span from `traceparent`, or empty.
    pub parent_span_id: String,
    /// Exported (`crate::otel`).
    pub sampled: bool,
    /// gRPC path.
    pub method: String,
    /// Start (Unix ms).
    pub start_ms: u64,
    /// Duration.
    pub duration_us: u64,
    /// Status code name (`Ok` when successful).
    pub code: String,
}

/// One metric sample: series key → value, at a time.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// When (Unix ms).
    pub at_ms: u64,
    /// `(name, labels)` → value.
    pub values: BTreeMap<(String, Vec<(String, String)>), f64>,
}

/// Per-server operator state: recent spans and metric samples.
#[derive(Default)]
pub struct OpsState {
    spans: Mutex<VecDeque<Span>>,
    samples: Mutex<VecDeque<Sample>>,
}

impl OpsState {
    /// Records a finished RPC.
    pub fn record_span(&self, span: Span) {
        if let Ok(mut s) = self.spans.lock() {
            if s.len() >= TRACE_CAPACITY {
                s.pop_front();
            }
            s.push_back(span);
        }
    }

    fn spans(&self) -> Vec<Span> {
        self.spans
            .lock()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Takes a metrics sample.
    pub fn sample(&self, at_ms: u64, text: &str) {
        let s = Sample {
            at_ms,
            values: parse_exposition(text),
        };
        if let Ok(mut q) = self.samples.lock() {
            if q.len() >= SAMPLE_CAPACITY {
                q.pop_front();
            }
            q.push_back(s);
        }
    }

    fn samples(&self) -> Vec<Sample> {
        self.samples
            .lock()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Parses Prometheus text exposition into series.
pub fn parse_exposition(text: &str) -> BTreeMap<(String, Vec<(String, String)>), f64> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let Some((series, value)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(v) = value.parse::<f64>() else {
            continue;
        };
        let (name, labels) = match series.split_once('{') {
            Some((n, rest)) => {
                let body = rest.trim_end_matches('}');
                let mut labels = Vec::new();
                for kv in split_labels(body) {
                    if let Some((k, v)) = kv.split_once('=') {
                        labels.push((k.to_string(), v.trim_matches('"').to_string()));
                    }
                }
                labels.sort();
                (n.to_string(), labels)
            }
            None => (series.to_string(), Vec::new()),
        };
        out.insert((name, labels), v);
    }
    out
}

/// Splits `a="x",b="y,z"` at commas outside quotes.
fn split_labels(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in body.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The fixed metric names (`crate::metrics`).
pub fn descriptors() -> Vec<v1::MetricDescriptor> {
    let d = |name: &str, t: v1::MetricType, unit: &str, help: &str, labels: &[&str]| {
        v1::MetricDescriptor {
            name: name.into(),
            r#type: t as i32,
            unit: unit.into(),
            help: help.into(),
            label_names: labels.iter().map(|l| l.to_string()).collect(),
        }
    };
    use v1::MetricType::{Counter, Gauge, Histogram};
    vec![
        d("bee_up", Gauge, "", "1 while the server runs", &[]),
        d(
            "bee_rpc_requests_total",
            Counter,
            "requests",
            "RPCs answered",
            &["method", "code"],
        ),
        d(
            "bee_rpc_duration_seconds",
            Histogram,
            "seconds",
            "RPC latency",
            &["method"],
        ),
        d(
            "bee_inflight_requests",
            Gauge,
            "requests",
            "RPCs in flight",
            &["tier"],
        ),
        d(
            "bee_admission_shed_total",
            Counter,
            "requests",
            "requests shed by admission control",
            &["tier", "why"],
        ),
        d(
            "bee_land_batches_total",
            Counter,
            "batches",
            "coalesced land batches",
            &[],
        ),
        d(
            "bee_land_batch_size",
            Histogram,
            "lands",
            "lands per coalesced batch",
            &[],
        ),
        d(
            "bee_lands_total",
            Counter,
            "lands",
            "lands by result",
            &["result"],
        ),
        d(
            "bee_land_cas_retries_total",
            Counter,
            "retries",
            "ref compare-and-swap retries",
            &[],
        ),
        d(
            "bee_coalescer_owner_changes_total",
            Counter,
            "changes",
            "coalescer ownership changes",
            &[],
        ),
        d(
            "bee_events_appended_total",
            Counter,
            "events",
            "events appended",
            &[],
        ),
        d(
            "bee_audit_entries_total",
            Counter,
            "entries",
            "audit entries written",
            &[],
        ),
        d(
            "bee_secret_findings_total",
            Counter,
            "findings",
            "push-protection findings",
            &["outcome"],
        ),
        d(
            "bee_git_requests_total",
            Counter,
            "requests",
            "git gateway requests",
            &["transport", "service", "outcome"],
        ),
        d(
            "bee_git_request_duration_seconds",
            Histogram,
            "seconds",
            "git gateway request latency",
            &["transport", "service"],
        ),
        d(
            "bee_git_fetch_protocol_total",
            Counter,
            "fetches",
            "git fetches by protocol version",
            &["transport", "version"],
        ),
        d(
            "bee_git_upload_pack_bytes_total",
            Counter,
            "bytes",
            "bytes sent by upload-pack",
            &["transport"],
        ),
        d(
            "bee_git_receive_pack_bytes_total",
            Counter,
            "bytes",
            "bytes received by receive-pack",
            &["transport"],
        ),
        d(
            "bee_git_pack_cache_total",
            Counter,
            "fetches",
            "pack cache hits and misses",
            &["result"],
        ),
        d(
            "bee_git_push_ref_updates_total",
            Counter,
            "refs",
            "ref updates of git pushes",
            &["result"],
        ),
        d(
            "bee_git_lfs_bytes_total",
            Counter,
            "bytes",
            "Git LFS object bytes",
            &["direction"],
        ),
    ]
}

fn matches(labels: &[(String, String)], m: &[v1::MetricLabelMatcher]) -> ApiResult<bool> {
    for x in m {
        let v = labels
            .iter()
            .find(|(k, _)| *k == x.name)
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        let ok = match v1::MetricMatchOp::try_from(x.op).unwrap_or(v1::MetricMatchOp::Equal) {
            v1::MetricMatchOp::Unspecified | v1::MetricMatchOp::Equal => v == x.value,
            v1::MetricMatchOp::NotEqual => v != x.value,
            v1::MetricMatchOp::Regex | v1::MetricMatchOp::NotRegex => {
                let re = regex_lite(&x.value)?;
                let hit = re.is_match(v);
                if x.op == v1::MetricMatchOp::Regex as i32 {
                    hit
                } else {
                    !hit
                }
            }
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A matcher regex, anchored as in PromQL.
fn regex_lite(p: &str) -> ApiResult<RegexMatcher> {
    RegexMatcher::new(p)
}

/// A tiny anchored pattern matcher for label regexes: `.*`, `.+`, `|` alternatives and
/// literal text (the subset label matchers use).
struct RegexMatcher {
    alts: Vec<Vec<Piece>>,
}

#[derive(Clone)]
enum Piece {
    Lit(char),
    Any,
    AnyRun { min: usize },
}

impl RegexMatcher {
    fn new(p: &str) -> ApiResult<RegexMatcher> {
        let mut alts = Vec::new();
        for alt in p.split('|') {
            let mut pieces = Vec::new();
            let cs: Vec<char> = alt.chars().collect();
            let mut i = 0;
            while i < cs.len() {
                match cs[i] {
                    '.' if cs.get(i + 1) == Some(&'*') => {
                        pieces.push(Piece::AnyRun { min: 0 });
                        i += 2;
                    }
                    '.' if cs.get(i + 1) == Some(&'+') => {
                        pieces.push(Piece::AnyRun { min: 1 });
                        i += 2;
                    }
                    '.' => {
                        pieces.push(Piece::Any);
                        i += 1;
                    }
                    '\\' if i + 1 < cs.len() => {
                        pieces.push(Piece::Lit(cs[i + 1]));
                        i += 2;
                    }
                    c if "[](){}*+?^$".contains(c) => {
                        return Err(ApiError::invalid(
                            "bad_matcher",
                            format!("label regex {p:?} uses unsupported syntax {c:?}"),
                        ));
                    }
                    c => {
                        pieces.push(Piece::Lit(c));
                        i += 1;
                    }
                }
            }
            alts.push(pieces);
        }
        Ok(RegexMatcher { alts })
    }

    fn is_match(&self, s: &str) -> bool {
        let cs: Vec<char> = s.chars().collect();
        self.alts.iter().any(|a| Self::m(a, &cs))
    }

    fn m(p: &[Piece], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(Piece::Lit(c)) => s.first() == Some(c) && Self::m(&p[1..], &s[1..]),
            Some(Piece::Any) => !s.is_empty() && Self::m(&p[1..], &s[1..]),
            Some(Piece::AnyRun { min }) => (*min..=s.len()).any(|k| Self::m(&p[1..], &s[k..])),
        }
    }
}

type Series = BTreeMap<(String, Vec<(String, String)>), f64>;

/// Evaluates one metric at one sample (and, for rates, an earlier one).
fn evaluate(
    name: &str,
    matchers: &[v1::MetricLabelMatcher],
    agg: i32,
    group_by: &[String],
    now: &Series,
    earlier: Option<(&Series, f64)>,
) -> ApiResult<BTreeMap<Vec<(String, String)>, f64>> {
    let agg = v1::MetricAggregation::try_from(agg).unwrap_or(v1::MetricAggregation::Unspecified);
    let group = |labels: &[(String, String)]| -> Vec<(String, String)> {
        if agg == v1::MetricAggregation::Unspecified {
            return labels.to_vec();
        }
        labels
            .iter()
            .filter(|(k, _)| group_by.contains(k))
            .cloned()
            .collect()
    };
    let mut out: BTreeMap<Vec<(String, String)>, Vec<f64>> = BTreeMap::new();
    match agg {
        v1::MetricAggregation::P50 | v1::MetricAggregation::P90 | v1::MetricAggregation::P99 => {
            let q = match agg {
                v1::MetricAggregation::P50 => 0.5,
                v1::MetricAggregation::P90 => 0.9,
                _ => 0.99,
            };
            // Buckets per group, as deltas over the window when an earlier sample exists.
            let bucket = format!("{name}_bucket");
            let mut by: BTreeMap<Vec<(String, String)>, BTreeMap<String, f64>> = BTreeMap::new();
            for ((n, labels), v) in now {
                if *n != bucket {
                    continue;
                }
                let base: Vec<(String, String)> =
                    labels.iter().filter(|(k, _)| k != "le").cloned().collect();
                if !matches(&base, matchers)? {
                    continue;
                }
                let le = labels
                    .iter()
                    .find(|(k, _)| k == "le")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let before = earlier
                    .and_then(|(e, _)| e.get(&(n.clone(), labels.clone())).copied())
                    .unwrap_or(0.0);
                *by.entry(group(&base)).or_default().entry(le).or_default() += v - before;
            }
            for (g, buckets) in by {
                let mut pts: Vec<(f64, f64)> = buckets
                    .iter()
                    .map(|(le, c)| {
                        (
                            if le == "+Inf" {
                                f64::INFINITY
                            } else {
                                le.parse().unwrap_or(f64::INFINITY)
                            },
                            *c,
                        )
                    })
                    .collect();
                pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                let total = pts.last().map(|p| p.1).unwrap_or(0.0);
                if total <= 0.0 {
                    continue;
                }
                let rank = q * total;
                let mut prev = (0.0, 0.0);
                let mut val = 0.0;
                for (le, c) in &pts {
                    if *c >= rank {
                        val = if le.is_infinite() {
                            prev.0
                        } else if *c - prev.1 > 0.0 {
                            prev.0 + (le - prev.0) * (rank - prev.1) / (c - prev.1)
                        } else {
                            *le
                        };
                        break;
                    }
                    prev = (*le, *c);
                }
                out.entry(g).or_default().push(val);
            }
        }
        _ => {
            for ((n, labels), v) in now {
                if n != name || !matches(labels, matchers)? {
                    continue;
                }
                let value = if agg == v1::MetricAggregation::Rate {
                    match earlier {
                        Some((e, secs)) if secs > 0.0 => {
                            let before =
                                e.get(&(n.clone(), labels.clone())).copied().unwrap_or(0.0);
                            ((v - before) / secs).max(0.0)
                        }
                        _ => 0.0,
                    }
                } else {
                    *v
                };
                out.entry(group(labels)).or_default().push(value);
            }
        }
    }
    Ok(out
        .into_iter()
        .map(|(g, vals)| {
            let v = match agg {
                v1::MetricAggregation::Avg => vals.iter().sum::<f64>() / vals.len().max(1) as f64,
                v1::MetricAggregation::Max => vals.iter().cloned().fold(f64::MIN, f64::max),
                v1::MetricAggregation::Min => vals.iter().cloned().fold(f64::MAX, f64::min),
                _ => vals.iter().sum(),
            };
            (g, v)
        })
        .collect())
}

/// A stored incident.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IncidentRec {
    /// The incident (proto3 JSON).
    pub incident: v1::OpsIncident,
}

/// The default status page components.
const DEFAULT_COMPONENTS: &[(&str, &str, &str)] = &[
    ("git", "Git operations", "Clone, fetch, push and land"),
    ("api", "API requests", "gRPC and HTTP/JSON"),
    (
        "change-requests",
        "Change requests",
        "Reviews, checks and merge queue",
    ),
    ("issues", "Issues", "Issues and projects"),
    ("webhooks", "Webhooks", "Event delivery"),
    ("actions", "Actions", "Workflows and runners"),
    ("search", "Search", "Code and forge search"),
    ("packages", "Packages", "Package registries"),
    ("pages", "Pages", "Static sites"),
];

/// The ops service.
pub struct OpsApi {
    state: Arc<AppState>,
}

impl OpsApi {
    /// The service over `state`; records this start as a rollout when the binary or config
    /// changed since the cell last ran.
    pub fn new(state: Arc<AppState>) -> OpsApi {
        let s = state.clone();
        tokio::spawn(async move {
            if let Err(e) = record_start(&s).await {
                logging::log(
                    Level::Warn,
                    "ops",
                    "recording the start rollout failed",
                    &[("error", e.detail.message.into())],
                );
            }
        });
        OpsApi { state }
    }
}

/// The effective config as TOML with secrets replaced by `<redacted>`.
pub fn redacted_config(cfg: &crate::config::Config) -> String {
    let mut v = toml::Value::try_from(cfg).unwrap_or(toml::Value::Table(Default::default()));
    redact(&mut v, "");
    toml::to_string(&v).unwrap_or_default()
}

fn secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.contains("secret")
        || k.contains("password")
        || k.contains("token")
        || k.ends_with("key_hex")
        || k.contains("private_key")
        || k == "s3_access_key_id"
}

fn redact(v: &mut toml::Value, key: &str) {
    match v {
        toml::Value::Table(t) => {
            for (k, x) in t.iter_mut() {
                redact(x, k);
            }
        }
        toml::Value::Array(a) => {
            for x in a.iter_mut() {
                redact(x, key);
            }
        }
        toml::Value::String(s) => {
            if secret_key(key) && !s.is_empty() {
                *s = "<redacted>".into();
            } else if (key.ends_with("_url") || key == "database") && s.contains('@') {
                // Credentials in a URL's userinfo.
                if let Some((scheme, rest)) = s.split_once("://")
                    && let Some((_, host)) = rest.rsplit_once('@')
                {
                    *s = format!("{scheme}://<redacted>@{host}");
                }
            }
        }
        _ => {}
    }
}

fn config_version(cfg: &crate::config::Config) -> String {
    let text = toml::to_string(cfg).unwrap_or_default();
    bee_core::hex::encode(&blake3::hash(text.as_bytes()).as_bytes()[..8])
}

const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

async fn record_start(state: &AppState) -> ApiResult<()> {
    let now = state.now_ms();
    for (kind, current) in [
        ("binary", BINARY_VERSION.to_string()),
        ("config", config_version(&state.cfg)),
    ] {
        let last: Option<String> = get_json(state.kv.as_ref(), NS_VERSION, kind).await?;
        if last.as_deref() == Some(current.as_str()) {
            continue;
        }
        let id = crate::util::new_ulid(now)?.to_text();
        let cell = v1::OpsRolloutCell {
            cell: state.cfg.server.cell.clone(),
            state: v1::OpsRolloutState::Succeeded as i32,
            nodes_updated: 1,
            nodes_total: 1,
            started_at: Some(timestamp_ms(now)),
            finished_at: Some(timestamp_ms(now)),
            ..Default::default()
        };
        let r = v1::OpsRollout {
            id: id.clone(),
            kind: kind.into(),
            from_version: last.unwrap_or_default(),
            to_version: current.clone(),
            state: v1::OpsRolloutState::Succeeded as i32,
            cells: vec![cell],
            reason: "server start".into(),
            created_at: Some(timestamp_ms(now)),
            updated_at: Some(timestamp_ms(now)),
            ..Default::default()
        };
        put_json(state.kv.as_ref(), NS_ROLLOUT, &id, &r).await?;
        put_json(state.kv.as_ref(), NS_VERSION, kind, &current).await?;
    }
    Ok(())
}

fn level_of(l: Level) -> v1::OpsLogLevel {
    match l {
        Level::Error => v1::OpsLogLevel::Error,
        Level::Warn => v1::OpsLogLevel::Warn,
        Level::Info => v1::OpsLogLevel::Info,
        Level::Debug => v1::OpsLogLevel::Debug,
    }
}

fn log_entry(state: &AppState, r: &logging::LogRecord) -> v1::OpsLogEntry {
    let field = |k: &str| {
        r.fields
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default()
    };
    v1::OpsLogEntry {
        time: Some(timestamp_ms(r.ts_ms)),
        level: level_of(r.level) as i32,
        component: r.component.clone(),
        message: r.msg.clone(),
        request_id: field("request_id"),
        trace_id: field("request_id"),
        principal_id: field("principal"),
        repo_id: field("repo"),
        rpc: field("method"),
        error_reason: field("reason"),
        path_hashes: Vec::new(),
        fields: r
            .fields
            .iter()
            .map(|(k, v)| v1::KeyValue {
                key: k.clone(),
                value: v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string()),
            })
            .collect(),
        node: state.instance_id.clone(),
        cell: state.cfg.server.cell.clone(),
    }
}

fn log_matches(e: &v1::OpsLogEntry, f: &v1::OpsLogFilter) -> bool {
    (f.components.is_empty() || f.components.contains(&e.component))
        && (f.request_id.is_empty() || e.request_id == f.request_id)
        && (f.trace_id.is_empty() || e.trace_id == f.trace_id)
        && (f.principal_id.is_empty() || e.principal_id == f.principal_id)
        && (f.repo_id.is_empty() || e.repo_id == f.repo_id)
        && (f.min_level == 0 || e.level >= f.min_level)
        && (f.text.is_empty() || e.message.to_lowercase().contains(&f.text.to_lowercase()))
}

fn span_trace(s: &Span) -> v1::OpsTrace {
    let error = s.code != "Ok";
    v1::OpsTrace {
        trace_id: s.trace_id.clone(),
        spans: vec![v1::OpsTraceSpan {
            span_id: s.span_id.clone(),
            parent_span_id: s.parent_span_id.clone(),
            service: "bee-server".into(),
            operation: s.method.clone(),
            start: Some(timestamp_ms(s.start_ms)),
            duration_us: s.duration_us,
            error,
            error_reason: if error { s.code.clone() } else { String::new() },
            attributes: Vec::new(),
        }],
        root_service: "bee-server".into(),
        root_operation: s.method.clone(),
        duration_us: s.duration_us,
        error,
    }
}

impl OpsApi {
    async fn components(&self) -> ApiResult<Vec<v1::OpsStatusComponent>> {
        let s = &self.state;
        let mut stored: BTreeMap<String, v1::OpsStatusComponent> =
            scan_json::<v1::OpsStatusComponent>(s.kv.as_ref(), NS_COMPONENT, "")
                .await?
                .into_iter()
                .map(|(_, c)| (c.id.clone(), c))
                .collect();
        for (id, name, desc) in DEFAULT_COMPONENTS {
            stored
                .entry((*id).to_string())
                .or_insert_with(|| v1::OpsStatusComponent {
                    id: (*id).into(),
                    name: (*name).into(),
                    description: (*desc).into(),
                    status: v1::OpsComponentStatus::Operational as i32,
                    updated_at: Some(timestamp_ms(s.started_ms())),
                });
        }
        Ok(stored.into_values().collect())
    }

    async fn set_component(&self, id: &str, status: i32) -> ApiResult<v1::OpsStatusComponent> {
        let s = &self.state;
        if v1::OpsComponentStatus::try_from(status).unwrap_or(v1::OpsComponentStatus::Unspecified)
            == v1::OpsComponentStatus::Unspecified
        {
            return Err(ApiError::invalid("missing_field", "status is required"));
        }
        let mut c = self
            .components()
            .await?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| ApiError::not_found(format!("status component {id}")))?;
        c.status = status;
        c.updated_at = Some(timestamp_ms(s.now_ms()));
        put_json(s.kv.as_ref(), NS_COMPONENT, &c.id, &c).await?;
        Ok(c)
    }

    async fn health(&self, include: bool) -> v1::GetOpsHealthResponse {
        let s = &self.state;
        let now = Some(timestamp_ms(s.now_ms()));
        let timeout = Duration::from_secs(2);
        let probe = |name: &str, ok: Result<(), String>, critical: bool| v1::HealthComponent {
            name: name.into(),
            liveness: v1::HealthStatus::Ok as i32,
            readiness: if ok.is_ok() {
                v1::HealthStatus::Ok
            } else {
                v1::HealthStatus::Down
            } as i32,
            reason: if ok.is_ok() {
                String::new()
            } else {
                "unreachable".into()
            },
            detail: ok.err().unwrap_or_default(),
            critical_path: critical,
            checked_at: now,
        };
        let flatten = |r: Result<Result<(), String>, tokio::time::error::Elapsed>| match r {
            Ok(x) => x,
            Err(_) => Err("probe timed out".into()),
        };
        let meta = flatten(
            tokio::time::timeout(timeout, async {
                s.meta
                    .org_by_name("-ops-probe-")
                    .await
                    .map(|_| ())
                    .map_err(|e| e.message)
            })
            .await,
        );
        let kv = flatten(
            tokio::time::timeout(timeout, async {
                s.kv.get("health", b"ops")
                    .await
                    .map(|_| ())
                    .map_err(|e| e.message)
            })
            .await,
        );
        let blobs = flatten(
            tokio::time::timeout(timeout, async {
                let org = bee_store::OrgId(bee_core::Ulid::from_bytes([0; 16]));
                s.blobs
                    .has(org, &[bee_object::blob_id(b"ops-probe")])
                    .await
                    .map(|_| ())
                    .map_err(|e| e.message)
            })
            .await,
        );
        let mut components = vec![
            probe("metadata", meta, true),
            probe("server_store", kv, true),
            probe("blob_store", blobs, true),
        ];
        let forge = match s.hub.get() {
            None if s.cfg.forge.enabled => Err("the forge did not open".to_string()),
            None => Ok(()),
            Some(w) => flatten(
                tokio::time::timeout(timeout, async {
                    w.forge
                        .orgs
                        .org_any(&bee_forge::Cx::system(), "-ops-probe-")
                        .await
                        .map(|_| ())
                        .or_else(|e| {
                            if e.reason == ErrorReason::NotFound {
                                Ok(())
                            } else {
                                Err(e.message)
                            }
                        })
                })
                .await,
            ),
        };
        components.push(probe("forge", forge, false));
        let search = match crate::services::search::index_of(s) {
            Some(ix) if ix.is_down() => Err(ix.last_error()),
            Some(_) => Ok(()),
            None if !s.cfg.search.enabled => Err("disabled".into()),
            None => Ok(()),
        };
        components.push(probe("search", search, false));
        let integrity = match s.metrics.last_integrity_failure() {
            Some(f) if s.now_ms().saturating_sub(f.at_ms) < crate::integrity::ALERT_WINDOW_MS => {
                Err(format!(
                    "{} integrity failure(s); latest {} ({}): {}",
                    s.metrics.integrity_failures(),
                    f.object,
                    f.source,
                    f.detail
                ))
            }
            _ => Ok(()),
        };
        let mut integrity = probe("integrity", integrity, false);
        if integrity.readiness == v1::HealthStatus::Down as i32 {
            integrity.reason = "integrity_failures".into();
        }
        components.push(integrity);
        components.push(self.jobs(now).await);
        let critical_down = components
            .iter()
            .any(|c| c.critical_path && c.readiness == v1::HealthStatus::Down as i32);
        let other_down = components
            .iter()
            .any(|c| !c.critical_path && c.readiness == v1::HealthStatus::Down as i32);
        v1::GetOpsHealthResponse {
            liveness: v1::HealthStatus::Ok as i32,
            readiness: if critical_down {
                v1::HealthStatus::Down
            } else if other_down {
                v1::HealthStatus::Degraded
            } else {
                v1::HealthStatus::Ok
            } as i32,
            components: if include { components } else { Vec::new() },
            node: s.instance_id.clone(),
            cell: s.cfg.server.cell.clone(),
            version: BINARY_VERSION.into(),
        }
    }

    /// The `background_jobs` component: running backups, restores and exports, and audit
    /// sinks that are failing (down when any sink fails).
    async fn jobs(&self, now: Option<pbjson_types::Timestamp>) -> v1::HealthComponent {
        let s = &self.state;
        let kv = s.kv.as_ref();
        let running_backups =
            scan_json::<crate::backup::BackupRec>(kv, crate::backup::NS_BACKUP, "")
                .await
                .map(|v| {
                    v.iter()
                        .filter(|(_, b)| b.state == v1::BackupState::Running as i32)
                        .count()
                })
                .unwrap_or(0);
        let running_restores =
            scan_json::<crate::backup::RestoreRec>(kv, crate::backup::NS_RESTORE, "")
                .await
                .map(|v| {
                    v.iter()
                        .filter(|(_, r)| r.state == v1::BackupState::Running as i32)
                        .count()
                })
                .unwrap_or(0);
        let running_exports = scan_json::<crate::services::export::ExportRec>(
            kv,
            crate::services::export::NS_EXPORT,
            "",
        )
        .await
        .map(|v| {
            v.iter()
                .filter(|(_, e)| {
                    e.state == v1::ExportState::Running as i32
                        || e.state == v1::ExportState::Queued as i32
                })
                .count()
        })
        .unwrap_or(0);
        let failing_after = s.cfg.audit.sink_failing_after.max(1);
        let sinks = scan_json::<crate::services::audit_sinks::SinkRec>(
            kv,
            crate::services::audit_sinks::NS_SINK,
            "",
        )
        .await
        .unwrap_or_default();
        let failing: Vec<String> = sinks
            .iter()
            .filter(|(_, x)| x.enabled && x.failures >= failing_after)
            .map(|(_, x)| x.name.clone())
            .collect();
        let detail = format!(
            "{running_backups} backups, {running_restores} restores and {running_exports} exports running; {} of {} audit sinks failing{}",
            failing.len(),
            sinks.len(),
            if failing.is_empty() {
                String::new()
            } else {
                format!(" ({})", failing.join(", "))
            }
        );
        v1::HealthComponent {
            name: "background_jobs".into(),
            liveness: v1::HealthStatus::Ok as i32,
            readiness: if failing.is_empty() {
                v1::HealthStatus::Ok
            } else {
                v1::HealthStatus::Down
            } as i32,
            reason: if failing.is_empty() {
                String::new()
            } else {
                "audit_sink_failing".into()
            },
            detail,
            critical_path: false,
            checked_at: now,
        }
    }

    fn check_cell(&self, cell: &str) -> ApiResult<()> {
        if cell.is_empty() || cell == self.state.cfg.server.cell {
            Ok(())
        } else {
            Err(ApiError::invalid(
                "unknown_cell",
                format!(
                    "this server serves cell {:?}; ask that cell's servers",
                    self.state.cfg.server.cell
                ),
            ))
        }
    }

    fn query(&self, r: &v1::QueryMetricsRequest) -> ApiResult<v1::QueryMetricsResponse> {
        let s = &self.state;
        self.check_cell(&r.cell)?;
        if !descriptors().iter().any(|d| d.name == r.name) {
            return Err(ApiError::invalid(
                "unknown_metric",
                format!("unknown metric {:?}", r.name),
            ));
        }
        let now_ms = s.now_ms();
        let at = r
            .time
            .as_ref()
            .map(|t| crate::util::ts_secs(t) * 1000)
            .unwrap_or(now_ms);
        let samples = s.ops.samples();
        let live = parse_exposition(&s.metrics.render());
        let current: &Series = if at + 1000 >= now_ms {
            &live
        } else {
            match samples.iter().rev().find(|x| x.at_ms <= at) {
                Some(x) => &x.values,
                None => return Ok(v1::QueryMetricsResponse { series: Vec::new() }),
            }
        };
        let window = if r.window_seconds == 0 {
            300
        } else {
            r.window_seconds
        } * 1000;
        let earlier = samples
            .iter()
            .rev()
            .find(|x| x.at_ms + window <= at)
            .or_else(|| samples.first().filter(|x| x.at_ms < at))
            .map(|x| (&x.values, (at.saturating_sub(x.at_ms)) as f64 / 1000.0));
        let vals = evaluate(
            &r.name,
            &r.matchers,
            r.aggregation,
            &r.group_by,
            current,
            earlier,
        )?;
        Ok(v1::QueryMetricsResponse {
            series: vals
                .into_iter()
                .map(|(labels, v)| v1::MetricSeries {
                    name: r.name.clone(),
                    labels: labels
                        .into_iter()
                        .map(|(k, v)| v1::KeyValue { key: k, value: v })
                        .collect(),
                    samples: vec![v1::MetricSample {
                        time: Some(timestamp_ms(at)),
                        value: v,
                    }],
                })
                .collect(),
        })
    }

    fn query_range(
        &self,
        r: &v1::QueryMetricsRangeRequest,
    ) -> ApiResult<v1::QueryMetricsRangeResponse> {
        let s = &self.state;
        self.check_cell(&r.cell)?;
        if !descriptors().iter().any(|d| d.name == r.name) {
            return Err(ApiError::invalid(
                "unknown_metric",
                format!("unknown metric {:?}", r.name),
            ));
        }
        let now = s.now_ms();
        let end = r
            .end
            .as_ref()
            .map(|t| crate::util::ts_secs(t) * 1000)
            .unwrap_or(now);
        let start = r
            .start
            .as_ref()
            .map(|t| crate::util::ts_secs(t) * 1000)
            .unwrap_or(end.saturating_sub(3_600_000));
        if start > end {
            return Err(ApiError::invalid("bad_range", "start is after end"));
        }
        let step = if r.step_seconds == 0 {
            60
        } else {
            r.step_seconds
        } * 1000;
        let points = (end - start) / step + 1;
        if points > 11_000 {
            let mut e = ApiError::new(
                ErrorReason::TooLarge,
                format!("{points} points; the limit is 11000"),
            );
            e.detail.payload = Some(v1::error_detail::Payload::LimitExceeded(
                v1::LimitExceeded {
                    limit: "metric_points".into(),
                    maximum: 11_000,
                    actual: points,
                },
            ));
            return Err(e);
        }
        let samples = s.ops.samples();
        let window = if r.window_seconds == 0 {
            300
        } else {
            r.window_seconds
        } * 1000;
        let mut series: BTreeMap<Vec<(String, String)>, Vec<v1::MetricSample>> = BTreeMap::new();
        let mut t = start;
        while t <= end {
            if let Some(cur) = samples.iter().rev().find(|x| x.at_ms <= t) {
                let earlier = samples
                    .iter()
                    .rev()
                    .find(|x| x.at_ms + window <= t)
                    .or_else(|| samples.first().filter(|x| x.at_ms < cur.at_ms))
                    .map(|x| {
                        (
                            &x.values,
                            (cur.at_ms.saturating_sub(x.at_ms)) as f64 / 1000.0,
                        )
                    });
                for (labels, v) in evaluate(
                    &r.name,
                    &r.matchers,
                    r.aggregation,
                    &r.group_by,
                    &cur.values,
                    earlier,
                )? {
                    series.entry(labels).or_default().push(v1::MetricSample {
                        time: Some(timestamp_ms(t)),
                        value: v,
                    });
                }
            }
            t += step;
        }
        Ok(v1::QueryMetricsRangeResponse {
            series: series
                .into_iter()
                .map(|(labels, samples)| v1::MetricSeries {
                    name: r.name.clone(),
                    labels: labels
                        .into_iter()
                        .map(|(k, v)| v1::KeyValue { key: k, value: v })
                        .collect(),
                    samples,
                })
                .collect(),
        })
    }

    async fn rollout(&self, id: &str) -> ApiResult<v1::OpsRollout> {
        if id.is_empty() {
            return Err(ApiError::invalid("missing_field", "id is required"));
        }
        get_json(self.state.kv.as_ref(), NS_ROLLOUT, id)
            .await?
            .ok_or_else(|| ApiError::not_found(format!("rollout {id}")))
    }

    async fn transition(
        &self,
        caller: &Caller,
        id: &str,
        to: v1::OpsRolloutState,
        reason: &str,
        meta: &RequestMeta,
    ) -> ApiResult<v1::OpsRollout> {
        let s = &self.state;
        let mut r = self.rollout(id).await?;
        let cur =
            v1::OpsRolloutState::try_from(r.state).unwrap_or(v1::OpsRolloutState::Unspecified);
        use v1::OpsRolloutState as S;
        let ok = match to {
            S::Paused => matches!(cur, S::Pending | S::InProgress),
            S::InProgress => cur == S::Paused,
            S::RollingBack => matches!(cur, S::Pending | S::InProgress | S::Paused),
            _ => false,
        };
        if !ok {
            return Err(ApiError::new(
                ErrorReason::InvalidState,
                format!(
                    "rollout {id} is {}; a finished rollout on this cell is undone by starting the previous version ({}), which records its own rollout",
                    cur.as_str_name(),
                    r.from_version
                ),
            ));
        }
        r.state = to as i32;
        r.updated_at = Some(timestamp_ms(s.now_ms()));
        if !reason.is_empty() {
            r.reason = reason.to_string();
        }
        for c in &mut r.cells {
            if !matches!(
                v1::OpsRolloutState::try_from(c.state),
                Ok(S::Succeeded) | Ok(S::RolledBack) | Ok(S::Failed)
            ) {
                c.state = to as i32;
            }
        }
        put_json(s.kv.as_ref(), NS_ROLLOUT, &r.id, &r).await?;
        s.audit(
            caller,
            meta,
            None,
            &r.id,
            "rollout.transition",
            json!({ "to": to.as_str_name() }),
        )
        .await?;
        Ok(r)
    }
}

macro_rules! ops_rpc {
    ($self:ident, $req:ident, |$caller:ident, $inner:ident, $meta:ident| $body:expr) => {
        $crate::unary!($self.state, $req, |$caller, $inner, $meta| $body)
    };
}

#[tonic::async_trait]
impl OpsService for OpsApi {
    type TailOpsLogsStream = RpcStream<v1::OpsLogEntry>;

    async fn get_ops_health(
        &self,
        req: tonic::Request<v1::GetOpsHealthRequest>,
    ) -> Rpc<v1::GetOpsHealthResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let _ = &caller;
            self.check_cell(&inner.cell)?;
            Ok::<_, ApiError>(self.health(inner.include_components).await)
        })
    }

    async fn get_ops_cluster_status(
        &self,
        req: tonic::Request<v1::GetOpsClusterStatusRequest>,
    ) -> Rpc<v1::OpsClusterStatus> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            require_operator(s, &caller).await?;
            let mine = &s.cfg.server.cell;
            let health = self.health(true).await;
            let meta_up = health
                .components
                .iter()
                .find(|c| c.name == "metadata")
                .is_some_and(|c| c.readiness == v1::HealthStatus::Ok as i32);
            let orgs = crate::services::support::all_orgs(s)
                .await
                .map(|v| v.len())
                .unwrap_or(0);
            // Serving stale reads: one was served in the last minute (SPEC §15.1).
            let (served_ms, served_age_ms) = s.last_refs.last_served();
            let serving_stale = served_ms > 0 && s.now_ms().saturating_sub(served_ms) < 60_000;
            let cells = if inner.cells.is_empty() || inner.cells.contains(mine) {
                vec![v1::OpsCell {
                    name: mine.clone(),
                    region: s.cfg.server.region.clone(),
                    version: BINARY_VERSION.into(),
                    config_version: config_version(&s.cfg),
                    schema_version: "1".into(),
                    status: health.readiness,
                    metadata_primary_up: meta_up,
                    serving_stale_reads: serving_stale,
                    stale_lag_ms: if serving_stale { served_age_ms } else { 0 },
                    nodes: 1,
                    orgs: orgs as u32,
                    redaction_epoch: 0,
                    policy_epoch: 0,
                }]
            } else {
                Vec::new()
            };
            Ok::<_, ApiError>(v1::OpsClusterStatus {
                cells,
                directory_up: true,
                directory_version: format!("single-cell:{mine}"),
                observed_at: Some(timestamp_ms(s.now_ms())),
            })
        })
    }

    async fn list_metric_descriptors(
        &self,
        req: tonic::Request<v1::ListMetricDescriptorsRequest>,
    ) -> Rpc<v1::ListMetricDescriptorsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            Ok::<_, ApiError>(v1::ListMetricDescriptorsResponse {
                descriptors: descriptors()
                    .into_iter()
                    .filter(|d| d.name.starts_with(&inner.prefix))
                    .collect(),
            })
        })
    }

    async fn query_metrics(
        &self,
        req: tonic::Request<v1::QueryMetricsRequest>,
    ) -> Rpc<v1::QueryMetricsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            self.query(&inner)
        })
    }

    async fn query_metrics_range(
        &self,
        req: tonic::Request<v1::QueryMetricsRangeRequest>,
    ) -> Rpc<v1::QueryMetricsRangeResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            self.query_range(&inner)
        })
    }

    async fn query_ops_logs(
        &self,
        req: tonic::Request<v1::QueryOpsLogsRequest>,
    ) -> Rpc<v1::QueryOpsLogsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            require_operator(s, &caller).await?;
            let f = inner.filter.clone().unwrap_or_default();
            self.check_cell(&f.cell)?;
            let start = inner
                .start
                .as_ref()
                .map(|t| crate::util::ts_secs(t) * 1000)
                .unwrap_or(0);
            let end = inner
                .end
                .as_ref()
                .map(|t| crate::util::ts_secs(t) * 1000)
                .unwrap_or(u64::MAX);
            let mut all: Vec<v1::OpsLogEntry> = logging::recent()
                .iter()
                .filter(|r| r.ts_ms >= start && r.ts_ms < end)
                .map(|r| log_entry(s, r))
                .filter(|e| log_matches(e, &f))
                .collect();
            if inner.newest_first {
                all.reverse();
            }
            let (token, size) = page_of(&inner.page);
            let (page, next) = offset_page(&all, &token, size)?;
            Ok::<_, ApiError>(v1::QueryOpsLogsResponse {
                entries: page,
                page: next_page(next),
            })
        })
    }

    async fn tail_ops_logs(
        &self,
        req: tonic::Request<v1::TailOpsLogsRequest>,
    ) -> Rpc<Self::TailOpsLogsStream> {
        let meta = RequestMeta::of(&req);
        let r = async {
            let caller = self.state.caller(req.metadata()).await?;
            require_operator(&self.state, &caller).await?;
            let inner = req.into_inner();
            let f = inner.filter.unwrap_or_default();
            self.check_cell(&f.cell)?;
            let state = self.state.clone();
            let backfill: Vec<v1::OpsLogEntry> = {
                let all: Vec<v1::OpsLogEntry> = logging::recent()
                    .iter()
                    .map(|r| log_entry(&state, r))
                    .filter(|e| log_matches(e, &f))
                    .collect();
                let n = (inner.backfill as usize).min(all.len());
                all[all.len() - n..].to_vec()
            };
            let mut rx = logging::subscribe();
            let (tx, out) =
                tokio::sync::mpsc::channel::<Result<v1::OpsLogEntry, tonic::Status>>(256);
            tokio::spawn(async move {
                for e in backfill {
                    if tx.send(Ok(e)).await.is_err() {
                        return;
                    }
                }
                loop {
                    match rx.recv().await {
                        Ok(rec) => {
                            let e = log_entry(&state, &rec);
                            if log_matches(&e, &f) && tx.send(Ok(e)).await.is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    }
                }
            });
            let s: Self::TailOpsLogsStream = Box::pin(tokio_stream_like(out));
            Ok::<_, ApiError>(tonic::Response::new(s))
        }
        .await;
        crate::services::object::finish(&meta, r)
    }

    async fn get_ops_trace(
        &self,
        req: tonic::Request<v1::GetOpsTraceRequest>,
    ) -> Rpc<v1::OpsTrace> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            self.state
                .ops
                .spans()
                .iter()
                .rev()
                .find(|x| x.trace_id == inner.trace_id || x.request_id == inner.trace_id)
                .map(span_trace)
                .ok_or_else(|| {
                    ApiError::not_found(format!(
                        "trace {} (sampled out or expired)",
                        inner.trace_id
                    ))
                })
        })
    }

    async fn search_ops_traces(
        &self,
        req: tonic::Request<v1::SearchOpsTracesRequest>,
    ) -> Rpc<v1::SearchOpsTracesResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            let start = inner
                .start
                .as_ref()
                .map(|t| crate::util::ts_secs(t) * 1000)
                .unwrap_or(0);
            let end = inner
                .end
                .as_ref()
                .map(|t| crate::util::ts_secs(t) * 1000)
                .unwrap_or(u64::MAX);
            let all: Vec<v1::OpsTraceSummary> = self
                .state
                .ops
                .spans()
                .iter()
                .rev()
                .filter(|x| {
                    (inner.service.is_empty() || inner.service == "bee-server")
                        && (inner.operation.is_empty() || x.method.contains(&inner.operation))
                        && x.duration_us >= inner.min_duration_us
                        && (!inner.errors_only || x.code != "Ok")
                        && (inner.request_id.is_empty() || x.request_id == inner.request_id)
                        && inner.principal_id.is_empty()
                        && x.start_ms >= start
                        && x.start_ms < end
                })
                .map(|x| v1::OpsTraceSummary {
                    trace_id: x.request_id.clone(),
                    root_service: "bee-server".into(),
                    root_operation: x.method.clone(),
                    start: Some(timestamp_ms(x.start_ms)),
                    duration_us: x.duration_us,
                    span_count: 1,
                    error: x.code != "Ok",
                })
                .collect();
            let (token, size) = page_of(&inner.page);
            let (page, next) = offset_page(&all, &token, size)?;
            Ok::<_, ApiError>(v1::SearchOpsTracesResponse {
                traces: page,
                page: next_page(next),
            })
        })
    }

    async fn validate_ops_config(
        &self,
        req: tonic::Request<v1::ValidateOpsConfigRequest>,
    ) -> Rpc<v1::ValidateOpsConfigResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            let production = inner.component == "production";
            if !(inner.component.is_empty() || inner.component == "server" || production) {
                return Err(ApiError::invalid(
                    "unknown_component",
                    "this server validates `server` configuration, or `production` for the \
                     operational checklist",
                ));
            }
            let parsed = crate::config::Config::parse_with_env(&inner.toml, &|_| None)
                .and_then(|c| c.validate().map(|_| c));
            Ok::<_, ApiError>(match parsed {
                Ok(c) if production => {
                    // Operational validation before production (`crate::preflight`).
                    let checks = crate::preflight::production_checklist(&c);
                    let item = |ch: &crate::preflight::Check| v1::OpsConfigError {
                        key_path: ch.key_path.into(),
                        line: 0,
                        column: 0,
                        message: ch.message.clone(),
                        code: format!("production:{}", ch.id),
                    };
                    let failed = |sev| {
                        checks
                            .iter()
                            .filter(|ch| !ch.ok && ch.severity == sev)
                            .map(item)
                            .collect::<Vec<_>>()
                    };
                    v1::ValidateOpsConfigResponse {
                        valid: crate::preflight::passes(&checks),
                        errors: failed(crate::preflight::Severity::Error),
                        warnings: failed(crate::preflight::Severity::Warning),
                        normalized_toml: redacted_config(&c),
                    }
                }
                Ok(c) => v1::ValidateOpsConfigResponse {
                    valid: true,
                    errors: Vec::new(),
                    warnings: Vec::new(),
                    normalized_toml: redacted_config(&c),
                },
                Err(e) => {
                    let msg = e.0.clone();
                    let (line, column) = line_col_of(&msg);
                    let code = if msg.contains("unknown field") {
                        "unknown_key"
                    } else if msg.contains("invalid type") {
                        "type_mismatch"
                    } else if msg.contains("environment variable") {
                        "unset_variable"
                    } else {
                        "invalid_value"
                    };
                    v1::ValidateOpsConfigResponse {
                        valid: false,
                        errors: vec![v1::OpsConfigError {
                            key_path: key_path_of(&msg),
                            line,
                            column,
                            message: msg,
                            code: code.into(),
                        }],
                        warnings: Vec::new(),
                        normalized_toml: String::new(),
                    }
                }
            })
        })
    }

    async fn get_effective_ops_config(
        &self,
        req: tonic::Request<v1::GetEffectiveOpsConfigRequest>,
    ) -> Rpc<v1::GetEffectiveOpsConfigResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            require_operator(s, &caller).await?;
            self.check_cell(&inner.cell)?;
            if !inner.node.is_empty() && inner.node != s.instance_id {
                return Err(ApiError::not_found(format!(
                    "node {} (this node is {})",
                    inner.node, s.instance_id
                )));
            }
            Ok::<_, ApiError>(v1::GetEffectiveOpsConfigResponse {
                toml: redacted_config(&s.cfg),
                config_version: config_version(&s.cfg),
                applied_at: Some(timestamp_ms(s.started_ms())),
            })
        })
    }

    async fn get_rate_limit_status(
        &self,
        req: tonic::Request<v1::GetRateLimitStatusRequest>,
    ) -> Rpc<v1::GetRateLimitStatusResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let _ = inner;
            let s = &self.state;
            let now = s.now_ms();
            let buckets = s
                .limiter
                .peek(caller.id(), caller.root_id())
                .into_iter()
                .map(|(name, scope, rate, left)| v1::OpsRateLimitBucket {
                    name: name.into(),
                    scope: scope.into(),
                    scope_id: if scope == "principal" {
                        caller.id().to_string()
                    } else {
                        caller.root_id().to_string()
                    },
                    limit: (rate * 2.0) as u64,
                    remaining: left.floor() as u64,
                    window_seconds: 1,
                    reset_at: Some(timestamp_ms(if rate > 0.0 {
                        now + (((rate * 2.0 - left).max(0.0) / rate) * 1000.0) as u64
                    } else {
                        now
                    })),
                    cost_per_request: 1,
                })
                .collect();
            Ok::<_, ApiError>(v1::GetRateLimitStatusResponse {
                buckets,
                priority_tier: "interactive".into(),
                limiter_healthy: true,
            })
        })
    }

    async fn list_ops_rollouts(
        &self,
        req: tonic::Request<v1::ListOpsRolloutsRequest>,
    ) -> Rpc<v1::ListOpsRolloutsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            let mut all: Vec<v1::OpsRollout> =
                scan_json::<v1::OpsRollout>(self.state.kv.as_ref(), NS_ROLLOUT, "")
                    .await?
                    .into_iter()
                    .map(|(_, r)| r)
                    .filter(|r| inner.state == 0 || r.state == inner.state)
                    .filter(|r| {
                        inner.cell.is_empty() || r.cells.iter().any(|c| c.cell == inner.cell)
                    })
                    .collect();
            all.reverse();
            let (token, size) = page_of(&inner.page);
            let (page, next) = offset_page(&all, &token, size)?;
            Ok::<_, ApiError>(v1::ListOpsRolloutsResponse {
                rollouts: page,
                page: next_page(next),
            })
        })
    }

    async fn get_ops_rollout(
        &self,
        req: tonic::Request<v1::GetOpsRolloutRequest>,
    ) -> Rpc<v1::OpsRollout> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            self.rollout(&inner.id).await
        })
    }

    async fn pause_ops_rollout(
        &self,
        req: tonic::Request<v1::PauseOpsRolloutRequest>,
    ) -> Rpc<v1::OpsRollout> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            self.transition(
                &caller,
                &inner.id,
                v1::OpsRolloutState::Paused,
                &inner.reason,
                &meta,
            )
            .await
        })
    }

    async fn resume_ops_rollout(
        &self,
        req: tonic::Request<v1::ResumeOpsRolloutRequest>,
    ) -> Rpc<v1::OpsRollout> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            self.transition(
                &caller,
                &inner.id,
                v1::OpsRolloutState::InProgress,
                "",
                &meta,
            )
            .await
        })
    }

    async fn rollback_ops_rollout(
        &self,
        req: tonic::Request<v1::RollbackOpsRolloutRequest>,
    ) -> Rpc<v1::OpsRollout> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            self.transition(
                &caller,
                &inner.id,
                v1::OpsRolloutState::RollingBack,
                &inner.reason,
                &meta,
            )
            .await
        })
    }

    async fn list_ops_incidents(
        &self,
        req: tonic::Request<v1::ListOpsIncidentsRequest>,
    ) -> Rpc<v1::ListOpsIncidentsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let _ = &caller;
            let since = inner
                .since
                .as_ref()
                .map(|t| crate::util::ts_secs(t) * 1000)
                .unwrap_or(0);
            let mut all: Vec<v1::OpsIncident> =
                scan_json::<IncidentRec>(self.state.kv.as_ref(), NS_INCIDENT, "")
                    .await?
                    .into_iter()
                    .map(|(_, r)| r.incident)
                    .filter(|i| inner.state == 0 || i.state == inner.state)
                    .filter(|i| {
                        i.started_at
                            .as_ref()
                            .map(|t| crate::util::ts_secs(t) * 1000)
                            .unwrap_or(0)
                            >= since
                    })
                    .collect();
            all.reverse();
            let (token, size) = page_of(&inner.page);
            let (page, next) = offset_page(&all, &token, size)?;
            Ok::<_, ApiError>(v1::ListOpsIncidentsResponse {
                incidents: page,
                page: next_page(next),
            })
        })
    }

    async fn create_ops_incident(
        &self,
        req: tonic::Request<v1::CreateOpsIncidentRequest>,
    ) -> Rpc<v1::OpsIncident> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            require_operator(s, &caller).await?;
            if inner.title.trim().is_empty() {
                return Err(ApiError::invalid("missing_field", "title is required"));
            }
            let impact = if inner.impact.is_empty() {
                "minor".to_string()
            } else {
                inner.impact.clone()
            };
            if !matches!(impact.as_str(), "none" | "minor" | "major" | "critical") {
                return Err(ApiError::invalid(
                    "bad_impact",
                    "impact is none, minor, major or critical",
                ));
            }
            let known = self.components().await?;
            for c in &inner.component_ids {
                if !known.iter().any(|k| &k.id == c) {
                    return Err(ApiError::invalid(
                        "unknown_component",
                        format!("no status component {c:?}"),
                    ));
                }
            }
            let now = s.now_ms();
            let author =
                crate::services::auth::principal_to(&caller.record.clone().unwrap_or_else(|| {
                    crate::auth::PrincipalRecord {
                        id: caller.id().to_string(),
                        kind: "human".into(),
                        display: caller.id().to_string(),
                        org: String::new(),
                        owner: String::new(),
                        runtime: String::new(),
                        purpose: String::new(),
                        external_ids: Vec::new(),
                        groups: Vec::new(),
                        deactivated: false,
                    }
                }));
            let inc = v1::OpsIncident {
                id: crate::util::new_ulid(now)?.to_text(),
                title: inner.title.trim().to_string(),
                state: v1::OpsIncidentState::Investigating as i32,
                impact,
                component_ids: inner.component_ids.clone(),
                cells: if inner.cells.is_empty() {
                    vec![s.cfg.server.cell.clone()]
                } else {
                    inner.cells.clone()
                },
                updates: vec![v1::OpsIncidentUpdate {
                    state: v1::OpsIncidentState::Investigating as i32,
                    body: inner.body.clone(),
                    author: Some(author),
                    posted_at: Some(timestamp_ms(now)),
                }],
                started_at: Some(timestamp_ms(now)),
                resolved_at: None,
            };
            if inner.component_status != 0 {
                for c in &inner.component_ids {
                    self.set_component(c, inner.component_status).await?;
                }
            }
            put_json(
                s.kv.as_ref(),
                NS_INCIDENT,
                &inc.id,
                &IncidentRec {
                    incident: inc.clone(),
                },
            )
            .await?;
            s.audit(
                &caller,
                &meta,
                None,
                &inc.id,
                "incident.create",
                json!({ "impact": inc.impact }),
            )
            .await?;
            Ok::<_, ApiError>(inc)
        })
    }

    async fn update_ops_incident(
        &self,
        req: tonic::Request<v1::UpdateOpsIncidentRequest>,
    ) -> Rpc<v1::OpsIncident> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            require_operator(s, &caller).await?;
            if inner.body.trim().is_empty() {
                return Err(ApiError::invalid("missing_field", "body is required"));
            }
            let mut rec: IncidentRec = get_json(s.kv.as_ref(), NS_INCIDENT, &inner.id)
                .await?
                .ok_or_else(|| ApiError::not_found(format!("incident {}", inner.id)))?;
            let inc = &mut rec.incident;
            if inc.state == v1::OpsIncidentState::Resolved as i32 && inner.state.is_none() {
                return Err(ApiError::new(
                    ErrorReason::InvalidState,
                    "the incident is resolved",
                ));
            }
            let now = s.now_ms();
            if let Some(st) = inner.state {
                inc.state = st;
                if st == v1::OpsIncidentState::Resolved as i32 {
                    inc.resolved_at = Some(timestamp_ms(now));
                }
            }
            if let Some(i) = &inner.impact {
                inc.impact = i.clone();
            }
            if !inner.component_ids.is_empty() {
                inc.component_ids = inner.component_ids.clone();
            }
            inc.updates.push(v1::OpsIncidentUpdate {
                state: inc.state,
                body: inner.body.clone(),
                author: Some(v1::Principal {
                    id: caller.id().to_string(),
                    ..Default::default()
                }),
                posted_at: Some(timestamp_ms(now)),
            });
            if let Some(cs) = inner.component_status {
                for c in inc.component_ids.clone() {
                    self.set_component(&c, cs).await?;
                }
            }
            let out = inc.clone();
            put_json(s.kv.as_ref(), NS_INCIDENT, &out.id, &rec).await?;
            s.audit(
                &caller,
                &meta,
                None,
                &out.id,
                "incident.update",
                json!({ "state": out.state }),
            )
            .await?;
            Ok::<_, ApiError>(out)
        })
    }

    async fn list_ops_status_components(
        &self,
        req: tonic::Request<v1::ListOpsStatusComponentsRequest>,
    ) -> Rpc<v1::ListOpsStatusComponentsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let _ = (&caller, inner);
            Ok::<_, ApiError>(v1::ListOpsStatusComponentsResponse {
                components: self.components().await?,
            })
        })
    }

    async fn set_ops_status_component(
        &self,
        req: tonic::Request<v1::SetOpsStatusComponentRequest>,
    ) -> Rpc<v1::OpsStatusComponent> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            require_operator(s, &caller).await?;
            let c = self.set_component(&inner.id, inner.status).await?;
            s.audit(
                &caller,
                &meta,
                None,
                &c.id,
                "status_component.set",
                json!({ "status": c.status }),
            )
            .await?;
            Ok::<_, ApiError>(c)
        })
    }

    async fn list_ops_backups(
        &self,
        req: tonic::Request<v1::ListOpsBackupsRequest>,
    ) -> Rpc<v1::ListOpsBackupsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            require_operator(s, &caller).await?;
            if !(inner.kind.is_empty() || inner.kind == "metadata_snapshot") {
                return Ok(v1::ListOpsBackupsResponse {
                    backups: Vec::new(),
                    page: next_page(String::new()),
                });
            }
            let all: Vec<v1::OpsBackup> = crate::backup::list(s)
                .await?
                .into_iter()
                .filter(|b| inner.cell.is_empty() || b.cell == inner.cell)
                .filter(|b| b.state == v1::BackupState::Succeeded as i32)
                .map(|b| v1::OpsBackup {
                    id: b.id,
                    cell: b.cell,
                    kind: "metadata_snapshot".into(),
                    size_bytes: b.size_bytes,
                    verified: b.verified,
                    location: b.location,
                    started_at: Some(timestamp_ms(b.started_ms)),
                    finished_at: (b.finished_ms > 0).then(|| timestamp_ms(b.finished_ms)),
                })
                .collect();
            let (token, size) = page_of(&inner.page);
            let (page, next) = offset_page(&all, &token, size)?;
            Ok::<_, ApiError>(v1::ListOpsBackupsResponse {
                backups: page,
                page: next_page(next),
            })
        })
    }

    async fn list_ops_restore_drills(
        &self,
        req: tonic::Request<v1::ListOpsRestoreDrillsRequest>,
    ) -> Rpc<v1::ListOpsRestoreDrillsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            let s = &self.state;
            require_operator(s, &caller).await?;
            let mut all: Vec<v1::OpsRestoreDrill> = scan_json::<crate::backup::RestoreRec>(
                s.kv.as_ref(),
                crate::backup::NS_RESTORE,
                "",
            )
            .await?
            .into_iter()
            .map(|(_, r)| r)
            .filter(|r| r.drill && r.state != v1::BackupState::Running as i32)
            .filter(|_| inner.cell.is_empty() || inner.cell == s.cfg.server.cell)
            .map(|r| v1::OpsRestoreDrill {
                id: r.id.clone(),
                cell: s.cfg.server.cell.clone(),
                backup_id: r.backup_id.clone(),
                passed: r.state == v1::BackupState::Succeeded as i32
                    && r.fsck_clean
                    && r.conformance_passed,
                restore_duration_seconds: r.finished_ms.saturating_sub(r.started_ms) / 1000,
                data_loss_seconds: r.started_ms.saturating_sub(r.backup_ms) / 1000,
                failure: if r.state == v1::BackupState::Succeeded as i32
                    && r.fsck_clean
                    && r.conformance_passed
                {
                    String::new()
                } else {
                    r.detail.clone()
                },
                ran_at: Some(timestamp_ms(r.started_ms)),
            })
            .collect();
            all.reverse();
            let (token, size) = page_of(&inner.page);
            let (page, next) = offset_page(&all, &token, size)?;
            Ok::<_, ApiError>(v1::ListOpsRestoreDrillsResponse {
                drills: page,
                page: next_page(next),
            })
        })
    }

    async fn get_ops_cache_stats(
        &self,
        req: tonic::Request<v1::GetOpsCacheStatsRequest>,
    ) -> Rpc<v1::GetOpsCacheStatsResponse> {
        ops_rpc!(self, req, |caller, inner, meta| async {
            require_operator(&self.state, &caller).await?;
            self.check_cell(&inner.cell)?;
            if !matches!(inner.tier.as_str(), "" | "daemon" | "node" | "regional") {
                return Err(ApiError::invalid(
                    "unknown_tier",
                    "tier is daemon, node or regional",
                ));
            }
            // No cache tier runs inside bee-server; daemons and regional caches report
            // through their own GetTierStats.
            Ok::<_, ApiError>(v1::GetOpsCacheStatsResponse { tiers: Vec::new() })
        })
    }
}

/// A receiver as a response stream.
fn tokio_stream_like<T: Send + 'static>(
    mut rx: tokio::sync::mpsc::Receiver<T>,
) -> impl futures_util::Stream<Item = T> + Send + 'static {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// `line N column M` from a TOML parse message.
fn line_col_of(msg: &str) -> (u32, u32) {
    let num_after = |word: &str| -> u32 {
        msg.find(word)
            .map(|i| {
                msg[i + word.len()..]
                    .trim_start()
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    (num_after("line"), num_after("column"))
}

/// The key path a config message names (`config: <key>: ...` or a quoted field).
fn key_path_of(msg: &str) -> String {
    let m = msg.trim_start_matches("config: ");
    if let Some(i) = m.find("unknown field `") {
        let rest = &m[i + "unknown field `".len()..];
        return rest.split('`').next().unwrap_or("").to_string();
    }
    m.split_whitespace()
        .next()
        .filter(|w| w.contains('.'))
        .map(|w| w.trim_end_matches(':').to_string())
        .unwrap_or_default()
}

/// The metrics sampler.
pub fn spawn_sampler(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let every = Duration::from_millis(if state.cfg.ops.metrics_sample_ms == 0 {
            10_000
        } else {
            state.cfg.ops.metrics_sample_ms.max(10)
        });
        loop {
            state.ops.sample(state.now_ms(), &state.metrics.render());
            tokio::time::sleep(every).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_parses_and_quantiles_interpolate() {
        let m = crate::metrics::Metrics::new();
        for _ in 0..10 {
            m.rpc("/x", "Ok", 0.004);
        }
        m.rpc("/x", "Ok", 0.2);
        let series = parse_exposition(&m.render());
        assert!(series.contains_key(&(
            "bee_rpc_requests_total".to_string(),
            vec![("code".into(), "Ok".into()), ("method".into(), "/x".into())]
        )));
        let p50 = evaluate(
            "bee_rpc_duration_seconds",
            &[],
            v1::MetricAggregation::P50 as i32,
            &[],
            &series,
            None,
        )
        .unwrap();
        let v = *p50.values().next().unwrap();
        assert!(v > 0.0025 && v <= 0.005, "{v}");
        let sum = evaluate(
            "bee_rpc_requests_total",
            &[v1::MetricLabelMatcher {
                name: "method".into(),
                op: v1::MetricMatchOp::Regex as i32,
                value: "/.*".into(),
            }],
            v1::MetricAggregation::Sum as i32,
            &[],
            &series,
            None,
        )
        .unwrap();
        assert_eq!(*sum.values().next().unwrap(), 11.0);
    }

    #[test]
    fn config_secrets_are_redacted() {
        let mut c = crate::config::Config::default();
        c.blobs.s3_secret_access_key = "s3cr3t".into();
        c.metadata.postgres_url = "postgres://u:pw@db/x".into();
        let t = redacted_config(&c);
        assert!(!t.contains("s3cr3t"));
        assert!(!t.contains("pw@"));
        assert!(t.contains("<redacted>"));
        assert_eq!(
            key_path_of("config: unknown field `bogus`, expected"),
            "bogus"
        );
    }
}
