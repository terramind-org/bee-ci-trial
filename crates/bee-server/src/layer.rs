//! The per-request layer wrapped around every tonic service, on both the gRPC and the
//! HTTP/JSON surfaces: request ids, deadlines, admission control, metrics, request logs, and
//! the `bee-policy-epoch` / `bee-redaction-epoch` response trailers.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderValue};
use http_body::Frame;
use tonic::server::NamedService;
use tower::Service;

use crate::admission::Tier;
use crate::errors::ApiError;
use crate::logging::{self, Level};
use crate::state::{AppState, RequestMeta};
use bee_proto::v1::ErrorReason;

/// Response headers a handler sets that the layer moves into the trailers.
pub const EPOCH_HEADERS: [&str; 2] = ["bee-policy-epoch", "bee-redaction-epoch"];

/// The trailer naming the tier that answered.
pub const TIER_TRAILER: &str = "bee-tier";

/// A tonic service wrapped by the request layer.
#[derive(Clone)]
pub struct Layered<S> {
    inner: S,
    state: Arc<AppState>,
}

impl<S> Layered<S> {
    /// Wraps `inner`.
    pub fn new(inner: S, state: Arc<AppState>) -> Layered<S> {
        Layered { inner, state }
    }
}

impl<S: NamedService> NamedService for Layered<S> {
    const NAME: &'static str = S::NAME;
}

/// Parses a `grpc-timeout` header value (`<digits><unit>`).
pub fn parse_grpc_timeout(v: &str) -> Option<Duration> {
    if v.len() < 2 {
        return None;
    }
    let (num, unit) = v.split_at(v.len() - 1);
    let n: u64 = num.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n.saturating_mul(3600)),
        "M" => Duration::from_secs(n.saturating_mul(60)),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

/// Parses Connect's millisecond timeout header.
pub fn parse_connect_timeout(v: &str) -> Option<Duration> {
    v.parse::<u64>().ok().map(Duration::from_millis)
}

/// What bounds a call: how long the handler may take to answer, and, for a stream, how long
/// the response body may stay open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// How long the handler may take to produce its response (the headers of a stream).
    pub setup: Duration,
    /// When the client asked for a deadline: how long a streaming response stays open.
    /// `None` for calls that are not streams, and for streams whose client sent no deadline;
    /// those stay open until the client disconnects or the server shuts down.
    pub stream: Option<Duration>,
}

/// The deadlines of a call to `method` whose client asked for `asked` (`grpc-timeout`,
/// `Connect-Timeout-Ms`), under `limits` (SPEC §15.3).
///
/// Unary and client-streaming calls always end at `limits.default_deadline_ms` when the
/// client sent none. A server-streaming call is bounded only by a deadline the client sent:
/// a subscription that merely outlives the default deadline is not an error.
pub fn budget(
    method: &str,
    asked: Option<Duration>,
    limits: &crate::config::LimitsSection,
) -> Budget {
    use bee_proto::methods::MethodKind;
    let max = Duration::from_millis(limits.max_deadline_ms);
    let setup = asked
        .unwrap_or(Duration::from_millis(limits.default_deadline_ms))
        .min(max);
    let streaming = bee_proto::methods::find_method(method).is_some_and(|m| {
        matches!(
            m.kind,
            MethodKind::ServerStreaming | MethodKind::BidiStreaming
        )
    });
    Budget {
        setup,
        stream: if streaming {
            asked.map(|a| a.min(max))
        } else {
            None
        },
    }
}

/// Words telling a client how to resume `method` after its stream ended on its deadline.
fn resume_hint(method: &str) -> &'static str {
    match method {
        "/bee.v1.EventService/Subscribe" => {
            "subscribe again with from_seq set to the seq of the last event received plus one \
             to resume"
        }
        "/bee.v1.WorkflowService/TailWorkflowJobLogs" => {
            "tail again with offset set to the number of bytes of log already received to resume"
        }
        "/bee.v1.WorkflowService/WatchWorkflowRun" => {
            "watch again to resume; the first message carries the current snapshot of the run"
        }
        "/bee.v1.AgentService/StreamAgentRunEvents" => {
            "stream again with after_seq set to the seq of the last event received to resume"
        }
        "/bee.v1.AuditService/StreamAuditLog" => {
            "stream again with after_seq set to the seq of the last entry received to resume"
        }
        _ => "call the method again to resume",
    }
}

/// The error a stream ends with when the client's own deadline passes.
fn stream_deadline_error(method: &str, asked: Duration, request_id: &str) -> ApiError {
    ApiError::new(
        ErrorReason::DeadlineExceeded,
        format!(
            "deadline exceeded: the client timeout of {} ms elapsed; {}",
            asked.as_millis(),
            resume_hint(method)
        ),
    )
    .with_request_id(request_id)
}

/// A request body the layer can buffer and rebuild (to read a forge request's message for
/// admission before the handler decodes it).
pub trait Rebody: http_body::Body<Data = Bytes> + Sized + Send + 'static {
    /// A body holding `bytes`.
    fn from_bytes(bytes: Bytes) -> Self;
}

impl Rebody for tonic::body::Body {
    fn from_bytes(bytes: Bytes) -> Self {
        tonic::body::Body::new(http_body_util::Full::new(bytes))
    }
}

impl Rebody for http_body_util::Full<Bytes> {
    fn from_bytes(bytes: Bytes) -> Self {
        http_body_util::Full::new(bytes)
    }
}

/// True for an `Authorization` value that carries nothing: empty, or a scheme alone.
fn has_no_credential(v: &HeaderValue) -> bool {
    let v = v.to_str().unwrap_or("x").trim();
    v.is_empty() || v.eq_ignore_ascii_case("bearer") || v.eq_ignore_ascii_case("basic")
}

fn error_response(e: ApiError) -> http::Response<tonic::body::Body> {
    e.into_status().into_http::<tonic::body::Body>()
}

impl<S, B> Service<http::Request<B>> for Layered<S>
where
    S: Service<http::Request<B>, Response = http::Response<tonic::body::Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    B: Rebody,
    B::Error: std::fmt::Display + Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let state = self.state.clone();
        Box::pin(async move {
            let started = Instant::now();
            let method = req.uri().path().to_string();
            let request_id = req
                .headers()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty() && v.len() <= 128)
                .map(str::to_string)
                .unwrap_or_else(|| crate::util::random_hex(8));
            // OpenTelemetry: join the caller's trace or start one (`crate::otel`).
            let trace = req
                .headers()
                .get("traceparent")
                .and_then(|v| v.to_str().ok())
                .and_then(crate::otel::parse_traceparent)
                .unwrap_or_else(|| {
                    crate::otel::root_trace(
                        crate::util::random_hex(16),
                        state.cfg.tracing.sample_ratio,
                    )
                });
            let span_id = crate::util::random_hex(8);
            // A land forwarded to a coalescer owner that is not this process (its port was
            // taken over after a crash): refuse before anything else, the forwarder re-routes.
            if let Some(owner) = req
                .headers()
                .get(crate::coalescer::OWNER_HEADER)
                .and_then(|v| v.to_str().ok())
                && owner != state.instance_id
            {
                return Ok(error_response(
                    crate::coalescer::not_owner(format!(
                        "forwarded to coalescer owner {owner}, reached {}",
                        state.instance_id
                    ))
                    .with_request_id(&request_id),
                ));
            }
            let limits = &state.cfg.limits;
            let asked = req
                .headers()
                .get("grpc-timeout")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_grpc_timeout)
                .or_else(|| {
                    req.headers()
                        .get("connect-timeout-ms")
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_connect_timeout)
                });
            let budget = budget(&method, asked, limits);
            let deadline = started + budget.setup;
            // A blank credential (`Authorization: Bearer ` with no token) is no credential.
            let authenticated = req
                .headers()
                .get("authorization")
                .is_some_and(|v| !has_no_credential(v));
            let tier = Tier::classify(&method, authenticated);
            // A public read without credentials goes on as the anonymous read token
            // (`crate::access`), after it was classified into the anonymous tier.
            if !authenticated
                && crate::access::anonymous_read_method(&method)
                && let Ok(Some(token)) = state.anonymous_token()
                && let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}"))
            {
                req.headers_mut().insert("authorization", v);
            }
            req.extensions_mut().insert(RequestMeta {
                request_id: request_id.clone(),
                deadline,
                method: method.clone(),
            });
            let slot = match state.admission.acquire(tier, deadline).await {
                Ok(s) => s,
                Err(retry_ms) => {
                    state.metrics.shed(tier.as_str(), "concurrency");
                    state.metrics.rpc(
                        &method,
                        "RESOURCE_EXHAUSTED",
                        started.elapsed().as_secs_f64(),
                    );
                    return Ok(error_response(
                        ApiError::throttled(
                            format!("server busy: {} tier at capacity", tier.as_str()),
                            retry_ms,
                            &format!("concurrency:{}", tier.as_str()),
                        )
                        .with_request_id(&request_id),
                    ));
                }
            };
            state.metrics.inflight(tier.as_str(), 1);
            let mut forget = false;
            if let Some(w) = state.hub.get().cloned()
                && crate::forge::admission::inspects(&method)
            {
                let (parts, body) = req.into_parts();
                let (bytes, body) = if crate::forge::admission::single_request(&method) {
                    let limit = state.cfg.limits.max_message_bytes;
                    match http_body_util::BodyExt::collect(http_body_util::Limited::new(
                        body, limit,
                    ))
                    .await
                    {
                        Ok(c) => {
                            let b = c.to_bytes();
                            (Some(b.clone()), B::from_bytes(b))
                        }
                        Err(e) => {
                            state.metrics.inflight(tier.as_str(), -1);
                            drop(slot);
                            return Ok(crate::forge::admission::refuse(
                                ApiError::invalid("bad_body", format!("request body: {e}")),
                                &request_id,
                            ));
                        }
                    }
                } else {
                    (None, body)
                };
                if let Err(e) = crate::state::REQUEST_FORGE_DEADLINES
                    .scope(
                        (
                            deadline,
                            Duration::from_millis(state.cfg.limits.forge_lookup_deadline_ms),
                        ),
                        crate::forge::admission::check(
                            &state,
                            &w,
                            &parts.headers,
                            &method,
                            bytes.as_ref(),
                        ),
                    )
                    .await
                {
                    state.metrics.inflight(tier.as_str(), -1);
                    drop(slot);
                    state.metrics.rpc(
                        &method,
                        &format!("{:?}", e.clone().into_status().code()),
                        started.elapsed().as_secs_f64(),
                    );
                    return Ok(crate::forge::admission::refuse(e, &request_id));
                }
                forget = crate::forge::admission::ADMISSION_SERVICES
                    .contains(&crate::forge::admission::service_of(&method));
                if forget {
                    w.forget_admissions();
                }
                req = http::Request::from_parts(parts, body);
            }
            let access_change = crate::forge::admission::changes_access(&method);
            let lookup_deadline = Duration::from_millis(state.cfg.limits.forge_lookup_deadline_ms);
            let result = crate::state::REQUEST_FORGE_DEADLINES
                .scope(
                    (deadline, lookup_deadline),
                    tokio::time::timeout_at(deadline.into(), inner.call(req)),
                )
                .await;
            if forget && let Some(w) = state.hub.get() {
                w.forget_admissions();
            }
            if access_change {
                // Before the answer leaves: the next request through this server sees the
                // change (repo-access decisions, `crate::access`).
                state.forget_access();
            }
            state.metrics.inflight(tier.as_str(), -1);
            drop(slot);
            let mut resp = match result {
                Ok(Ok(r)) => r,
                Err(_) => {
                    let e = ApiError::new(ErrorReason::DeadlineExceeded, "deadline exceeded")
                        .with_request_id(&request_id);
                    error_response(e)
                }
            };
            let code = resp
                .headers()
                .get("grpc-status")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i32>().ok())
                .map(|c| format!("{:?}", tonic::Code::from_i32(c)))
                .unwrap_or_else(|| "Ok".to_string());
            let secs = started.elapsed().as_secs_f64();
            state.metrics.rpc(&method, &code, secs);
            let micros = started.elapsed().as_micros() as u64;
            let span = crate::services::ops::Span {
                request_id: request_id.clone(),
                trace_id: trace.trace_id.clone(),
                span_id: span_id.clone(),
                parent_span_id: trace.parent_span_id.clone().unwrap_or_default(),
                sampled: trace.sampled,
                method: method.clone(),
                start_ms: state.now_ms().saturating_sub(micros / 1000),
                duration_us: micros,
                code: code.clone(),
            };
            if trace.sampled
                && let Some(exporter) = &state.otel
            {
                exporter.push(span.clone());
            }
            state.ops.record_span(span);
            if let Ok(v) = HeaderValue::from_str(&crate::otel::format_context(
                &trace.trace_id,
                &span_id,
                trace.sampled,
            )) {
                resp.headers_mut().insert("traceresponse", v);
            }
            if state.cfg.logging.requests {
                logging::log(
                    Level::Info,
                    "rpc",
                    "request",
                    &[
                        ("method", method.clone().into()),
                        ("code", code.into()),
                        ("tier", tier.as_str().into()),
                        ("duration_ms", (secs * 1000.0).into()),
                        ("request_id", request_id.clone().into()),
                    ],
                );
            }
            if let Ok(v) = HeaderValue::from_str(&request_id) {
                resp.headers_mut().insert("x-request-id", v);
            }
            let mut extra = HeaderMap::new();
            for name in EPOCH_HEADERS {
                if let Some(v) = resp.headers_mut().remove(name) {
                    extra.insert(name, v);
                }
            }
            // Which tier answered (`docs/spec/cache-tiers.md` §1): caches answer `daemon` or
            // `regional`, and anything that reaches this layer was served by the server.
            extra.insert(TIER_TRAILER, HeaderValue::from_static("server"));
            let (parts, body) = resp.into_parts();
            // A stream is cut only at the deadline its client sent, with an error that says
            // how to resume; without one it stays open (`budget`).
            let cutoff = match budget.stream {
                Some(asked) if parts.status.is_success() => {
                    let (end, _) =
                        error_response(stream_deadline_error(&method, asked, &request_id))
                            .into_parts();
                    let mut trailers = end.headers;
                    trailers.remove(http::header::CONTENT_TYPE);
                    Some(Cutoff {
                        sleep: Box::pin(tokio::time::sleep_until((started + asked).into())),
                        trailers: Some(trailers),
                    })
                }
                _ => None,
            };
            Ok(http::Response::from_parts(
                parts,
                tonic::body::Body::new(TrailerBody {
                    inner: body,
                    extra: Some(extra),
                    cutoff,
                }),
            ))
        })
    }
}

/// Ends a streaming body at the client's deadline.
struct Cutoff {
    sleep: Pin<Box<tokio::time::Sleep>>,
    /// The closing trailers (`grpc-status: DEADLINE_EXCEEDED` and its message); `None` once sent.
    trailers: Option<HeaderMap>,
}

/// Appends extra headers to a gRPC body's trailers.
struct TrailerBody {
    inner: tonic::body::Body,
    extra: Option<HeaderMap>,
    cutoff: Option<Cutoff>,
}

impl http_body::Body for TrailerBody {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, tonic::Status>>> {
        if let Some(cut) = self.cutoff.as_mut() {
            if cut.trailers.is_none() {
                return Poll::Ready(None);
            }
            if cut.sleep.as_mut().poll(cx).is_ready() {
                let mut trailers = cut.trailers.take().unwrap_or_default();
                if let Some(extra) = self.extra.take() {
                    for (k, v) in extra.iter() {
                        trailers.insert(k.clone(), v.clone());
                    }
                }
                return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
            }
        }
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => match frame.into_trailers() {
                Ok(mut trailers) => {
                    if let Some(extra) = self.extra.take() {
                        for (k, v) in extra.iter() {
                            trailers.insert(k.clone(), v.clone());
                        }
                    }
                    Poll::Ready(Some(Ok(Frame::trailers(trailers))))
                }
                Err(frame) => Poll::Ready(Some(Ok(frame))),
            },
            Poll::Ready(None) => match self.extra.take() {
                Some(extra) => Poll::Ready(Some(Ok(Frame::trailers(extra)))),
                None => Poll::Ready(None),
            },
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.extra.is_none()
            && self.inner.is_end_stream()
            && self.cutoff.as_ref().is_none_or(|c| c.trailers.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_timeouts_parse() {
        assert_eq!(parse_grpc_timeout("100m"), Some(Duration::from_millis(100)));
        assert_eq!(parse_grpc_timeout("5S"), Some(Duration::from_secs(5)));
        assert_eq!(parse_grpc_timeout("1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_grpc_timeout("x"), None);
        assert_eq!(parse_grpc_timeout("10q"), None);
    }

    #[test]
    fn connect_timeouts_parse() {
        assert_eq!(
            parse_connect_timeout("100"),
            Some(Duration::from_millis(100))
        );
        assert_eq!(parse_connect_timeout("0"), Some(Duration::ZERO));
        assert_eq!(parse_connect_timeout("100ms"), None);
    }

    fn limits() -> crate::config::LimitsSection {
        crate::config::LimitsSection {
            default_deadline_ms: 500,
            max_deadline_ms: 2_000,
            ..Default::default()
        }
    }

    const UNARY: &str = "/bee.v1.IssueService/CreateIssue";
    const SUBSCRIBE: &str = "/bee.v1.EventService/Subscribe";
    const UPLOAD: &str = "/bee.v1.ObjectService/UploadObjects";

    #[test]
    fn unary_and_client_streaming_calls_end_at_the_default_deadline() {
        let l = limits();
        for m in [UNARY, UPLOAD] {
            let b = budget(m, None, &l);
            assert_eq!(b.setup, Duration::from_millis(500), "{m}");
            assert_eq!(b.stream, None, "{m}");
        }
        // A client deadline replaces the default, capped at the maximum.
        let b = budget(UNARY, Some(Duration::from_millis(100)), &l);
        assert_eq!(b.setup, Duration::from_millis(100));
        let b = budget(UNARY, Some(Duration::from_secs(60)), &l);
        assert_eq!(b.setup, Duration::from_secs(2));
        assert_eq!(b.stream, None);
    }

    #[test]
    fn a_server_stream_is_bounded_only_by_a_deadline_its_client_sent() {
        let l = limits();
        let b = budget(SUBSCRIBE, None, &l);
        assert_eq!(b.stream, None, "no client deadline: the stream stays open");
        assert_eq!(
            b.setup,
            Duration::from_millis(500),
            "setup is still bounded"
        );
        let b = budget(SUBSCRIBE, Some(Duration::from_millis(300)), &l);
        assert_eq!(b.stream, Some(Duration::from_millis(300)));
        let b = budget(SUBSCRIBE, Some(Duration::from_secs(60)), &l);
        assert_eq!(
            b.stream,
            Some(Duration::from_secs(2)),
            "capped at the maximum"
        );
    }

    #[test]
    fn every_server_stream_names_how_to_resume() {
        for m in bee_proto::methods::METHODS {
            if m.kind == bee_proto::methods::MethodKind::ServerStreaming {
                let e = stream_deadline_error(m.path, Duration::from_millis(7), "rid");
                let msg = e.into_status().message().to_string();
                assert!(
                    msg.starts_with("deadline exceeded: the client timeout of 7 ms elapsed; ")
                        && msg.contains("to resume"),
                    "{}: {msg}",
                    m.path
                );
            }
        }
    }

    #[test]
    fn a_scheme_without_a_token_is_no_credential() {
        for v in ["", "Bearer", "Bearer ", "bearer  ", "Basic"] {
            assert!(
                has_no_credential(&HeaderValue::from_str(v).unwrap()),
                "{v:?}"
            );
        }
        for v in ["Bearer abc", "Basic Z2l0OnQ=", "token"] {
            assert!(
                !has_no_credential(&HeaderValue::from_str(v).unwrap()),
                "{v:?}"
            );
        }
    }
}
