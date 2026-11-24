//! OpenTelemetry tracing (PRODUCT §4.22, `docs/spec/server.md` §6).
//!
//! Every RPC is one server span. A request carrying a W3C `traceparent` header joins that
//! trace: the span takes its trace id and records the caller's span as its parent, and the
//! caller's sampled flag decides export. A request without one starts a trace (a fresh 128-bit
//! id) sampled with probability `tracing.sample_ratio`, decided from the trace id so every
//! process agrees. Responses carry `traceresponse: 00-<trace id>-<span id>-<flags>` (W3C Trace
//! Context level 2) so a caller can find its span.
//!
//! With `tracing.otlp_endpoint` set, sampled spans are batched and exported every
//! `tracing.export_interval_ms` as OTLP/HTTP JSON (`POST <endpoint>/v1/traces`), with resource
//! attributes `service.name`, `service.instance.id` and `bee.cell`. A full queue drops the
//! oldest spans; an export that fails is dropped, never retried into a storm. Spans also stay
//! in the in-process ring behind `GetOpsTrace` and `SearchOpsTraces`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::services::ops::Span;

/// Spans held for export at most.
pub const QUEUE_CAPACITY: usize = 10_000;

/// A trace context taken from (or started for) one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceContext {
    /// 32 lowercase hex digits.
    pub trace_id: String,
    /// The caller's span (16 hex digits), when the request joined a trace.
    pub parent_span_id: Option<String>,
    /// Whether the span is exported.
    pub sampled: bool,
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parses a W3C `traceparent` value: `<version>-<trace id>-<parent id>-<flags>`. Version `ff`,
/// all-zero ids and malformed fields are refused; a future version is read by its first four
/// fields, as the specification asks.
pub fn parse_traceparent(value: &str) -> Option<TraceContext> {
    let value = value.trim();
    let mut parts = value.split('-');
    let (version, trace, parent, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if !is_hex(version, 2) || version == "ff" || (version == "00" && parts.next().is_some()) {
        return None;
    }
    if !is_hex(trace, 32) || !is_hex(parent, 16) || !is_hex(flags, 2) {
        return None;
    }
    if trace.bytes().all(|b| b == b'0') || parent.bytes().all(|b| b == b'0') {
        return None;
    }
    let flags = u8::from_str_radix(flags, 16).ok()?;
    Some(TraceContext {
        trace_id: trace.to_string(),
        parent_span_id: Some(parent.to_string()),
        sampled: flags & 1 == 1,
    })
}

/// A new root trace with id `trace_id` (32 hex digits), sampled when the id's first 64 bits
/// fall below `ratio` of the range.
pub fn root_trace(trace_id: String, ratio: f64) -> TraceContext {
    let head = u64::from_str_radix(&trace_id[..16.min(trace_id.len())], 16).unwrap_or(0);
    let sampled = ratio >= 1.0 || (ratio > 0.0 && (head as f64) < ratio * (u64::MAX as f64));
    TraceContext {
        trace_id,
        parent_span_id: None,
        sampled,
    }
}

/// `00-<trace>-<span>-<flags>`.
pub fn format_context(trace_id: &str, span_id: &str, sampled: bool) -> String {
    format!(
        "00-{trace_id}-{span_id}-{}",
        if sampled { "01" } else { "00" }
    )
}

fn attr(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

/// The OTLP/HTTP JSON body (`ExportTraceServiceRequest`) for `spans`.
pub fn otlp_body(spans: &[Span], service: &str, instance: &str, cell: &str) -> Value {
    let spans: Vec<Value> = spans
        .iter()
        .map(|s| {
            let (rpc_service, rpc_method) = s
                .method
                .trim_start_matches('/')
                .split_once('/')
                .unwrap_or((s.method.as_str(), ""));
            let start_ns = u128::from(s.start_ms) * 1_000_000;
            let end_ns = start_ns + u128::from(s.duration_us) * 1_000;
            let error = s.code != "Ok";
            let mut span = json!({
                "traceId": s.trace_id,
                "spanId": s.span_id,
                "name": s.method.trim_start_matches('/'),
                "kind": 2,
                "startTimeUnixNano": start_ns.to_string(),
                "endTimeUnixNano": end_ns.to_string(),
                "attributes": [
                    attr("rpc.system", "grpc"),
                    attr("rpc.service", rpc_service),
                    attr("rpc.method", rpc_method),
                    attr("bee.code", &s.code),
                    attr("bee.request_id", &s.request_id),
                ],
                "status": if error {
                    json!({ "code": 2, "message": s.code })
                } else {
                    json!({ "code": 1 })
                },
            });
            if !s.parent_span_id.is_empty() {
                span["parentSpanId"] = json!(s.parent_span_id);
            }
            span
        })
        .collect();
    json!({
        "resourceSpans": [{
            "resource": { "attributes": [
                attr("service.name", service),
                attr("service.instance.id", instance),
                attr("bee.cell", cell),
            ]},
            "scopeSpans": [{
                "scope": { "name": "bee-server", "version": env!("CARGO_PKG_VERSION") },
                "spans": spans,
            }],
        }]
    })
}

/// Batches sampled spans and posts them to an OTLP/HTTP collector.
pub struct Exporter {
    url: String,
    service: String,
    instance: String,
    cell: String,
    queue: Mutex<VecDeque<Span>>,
    /// Spans sent in a successful export.
    pub exported: AtomicU64,
    /// Spans dropped (queue full or a failed export).
    pub dropped: AtomicU64,
}

impl Exporter {
    /// An exporter to `<endpoint>/v1/traces`.
    pub fn new(endpoint: &str, service: &str, instance: &str, cell: &str) -> Exporter {
        Exporter {
            url: format!("{}/v1/traces", endpoint.trim_end_matches('/')),
            service: service.to_string(),
            instance: instance.to_string(),
            cell: cell.to_string(),
            queue: Mutex::new(VecDeque::new()),
            exported: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Queues a sampled span.
    pub fn push(&self, span: Span) {
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() >= QUEUE_CAPACITY {
            q.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        q.push_back(span);
    }

    fn take(&self) -> Vec<Span> {
        let mut q = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        q.drain(..).collect()
    }

    /// Sends what is queued now; returns how many spans went out.
    pub async fn flush(&self, sender: &bee_forge::events::delivery::Sender) -> usize {
        let spans = self.take();
        if spans.is_empty() {
            return 0;
        }
        let body = otlp_body(&spans, &self.service, &self.instance, &self.cell);
        let req = bee_forge::events::delivery::Prepared {
            body: body.to_string(),
            headers: [("content-type".to_string(), "application/json".to_string())]
                .into_iter()
                .collect(),
        };
        let out = sender.send(&self.url, &req).await;
        if out.success() {
            self.exported
                .fetch_add(spans.len() as u64, Ordering::Relaxed);
            spans.len()
        } else {
            self.dropped
                .fetch_add(spans.len() as u64, Ordering::Relaxed);
            crate::logging::log(
                crate::logging::Level::Warn,
                "otel",
                "trace export failed; spans dropped",
                &[
                    ("status", u64::from(out.status).into()),
                    ("error", out.error.into()),
                    ("spans", (spans.len() as u64).into()),
                ],
            );
            0
        }
    }

    /// Exports every `interval` until the exporter is dropped.
    pub fn spawn(self: &std::sync::Arc<Self>, interval: Duration) {
        let weak = std::sync::Arc::downgrade(self);
        tokio::spawn(async move {
            let sender = bee_forge::events::delivery::Sender::new(Duration::from_secs(10));
            loop {
                tokio::time::sleep(interval).await;
                let Some(exporter) = weak.upgrade() else {
                    break;
                };
                exporter.flush(&sender).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const P: &str = "00f067aa0ba902b7";

    #[test]
    fn traceparent_parsing_follows_w3c() {
        let c = parse_traceparent(&format!("00-{T}-{P}-01")).unwrap();
        assert_eq!(c.trace_id, T);
        assert_eq!(c.parent_span_id.as_deref(), Some(P));
        assert!(c.sampled);
        assert!(
            !parse_traceparent(&format!("00-{T}-{P}-00"))
                .unwrap()
                .sampled
        );
        // A future version with more fields is read; version 00 with more is not.
        assert!(parse_traceparent(&format!("01-{T}-{P}-01-extra")).is_some());
        for bad in [
            format!("00-{T}-{P}-01-extra"),
            format!("ff-{T}-{P}-01"),
            format!("00-{}-{P}-01", "0".repeat(32)),
            format!("00-{T}-{}-01", "0".repeat(16)),
            format!("00-{}-{P}-01", T.to_uppercase()),
            format!("00-{T}-{P}"),
            "garbage".to_string(),
        ] {
            assert!(parse_traceparent(&bad).is_none(), "{bad}");
        }
        assert_eq!(format_context(T, P, true), format!("00-{T}-{P}-01"));
    }

    #[test]
    fn root_sampling_is_a_function_of_the_trace_id() {
        assert!(root_trace(T.into(), 1.0).sampled);
        assert!(!root_trace(T.into(), 0.0).sampled);
        let low = format!("0000000000000001{}", &T[16..]);
        let high = format!("fffffffffffffff0{}", &T[16..]);
        assert!(root_trace(low.clone(), 0.5).sampled);
        assert!(!root_trace(high.clone(), 0.5).sampled);
        assert_eq!(root_trace(low.clone(), 0.5), root_trace(low, 0.5));
    }

    #[test]
    fn otlp_json_has_the_spans_and_the_resource() {
        let span = Span {
            request_id: "r1".into(),
            method: "/bee.v1.AuthService/WhoAmI".into(),
            start_ms: 1_000,
            duration_us: 2_500,
            code: "Ok".into(),
            trace_id: T.into(),
            span_id: "a1b2c3d4e5f60718".into(),
            parent_span_id: P.into(),
            sampled: true,
        };
        let body = otlp_body(&[span], "bee-server", "inst", "cell-a");
        let rs = &body["resourceSpans"][0];
        assert_eq!(
            rs["resource"]["attributes"][0]["value"]["stringValue"],
            "bee-server"
        );
        let s = &rs["scopeSpans"][0]["spans"][0];
        assert_eq!(s["traceId"], T);
        assert_eq!(s["parentSpanId"], P);
        assert_eq!(s["kind"], 2);
        assert_eq!(s["name"], "bee.v1.AuthService/WhoAmI");
        assert_eq!(s["startTimeUnixNano"], "1000000000");
        assert_eq!(s["endTimeUnixNano"], "1002500000");
        assert_eq!(s["status"]["code"], 1);
    }
}
