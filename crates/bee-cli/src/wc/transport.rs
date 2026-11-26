//! The working copy's [`bee_wc::Transport`] over the CLI's Connect HTTP/JSON client. It is
//! lazy: commands that stay local (`status`) never resolve an endpoint or open a connection.

use std::cell::OnceCell;

use bee_proto::v1::{self, ErrorDetail};
use prost::Message as _;
use serde_json::{Map, Value, json};

use crate::context::Context;
use crate::daemon_glue::{self, Link};
use crate::error::CliError;
use crate::rpc::{Client, Endpoint, StreamMode};

/// A lazily connected transport.
pub struct CliTransport {
    endpoint: Result<String, CliError>,
    token: Result<Option<String>, CliError>,
    timeout: Option<std::time::Duration>,
    ready: OnceCell<Result<(Client, tokio::runtime::Runtime), CliError>>,
    daemon_enabled: bool,
    daemon: OnceCell<Option<Link>>,
}

/// The one RPC the node daemon answers from its cache for a working copy.
const GET_OBJECTS: &str = "/bee.v1.ObjectService/GetObjects";

/// Streaming calls kept in flight at once when talking to the server directly.
const CONCURRENT_STREAMS: usize = 8;

/// The server-reported detail inside a CLI error (or one built from its fields).
pub fn detail_of(e: &CliError) -> Box<ErrorDetail> {
    let mut map: Map<String, Value> = e.payload.clone();
    map.insert("reason".into(), json!(e.reason));
    map.insert("message".into(), json!(e.message));
    map.insert("retryable".into(), json!(e.retryable));
    Box::new(
        serde_json::from_value(Value::Object(map)).unwrap_or_else(|_| ErrorDetail {
            reason: e.reason.clone(),
            message: e.message.clone(),
            retryable: e.retryable,
            subcode: e
                .payload
                .get("subcode")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            ..Default::default()
        }),
    )
}

fn parse(bytes: &[u8]) -> Result<Value, Box<ErrorDetail>> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_slice(bytes).map_err(|e| {
        detail_of(&CliError::internal(format!(
            "server sent a response that is not JSON: {e}"
        )))
    })
}

impl CliTransport {
    /// A transport for the endpoint chosen by [`super::endpoint_for`].
    pub fn new(ctx: &Context, wc_endpoint: Option<&str>) -> Self {
        Self {
            endpoint: super::endpoint_for(ctx, wc_endpoint),
            token: ctx.token(),
            timeout: ctx.options.timeout,
            ready: OnceCell::new(),
            daemon_enabled: daemon_glue::enabled(ctx.options.no_daemon),
            daemon: OnceCell::new(),
        }
    }

    /// The node daemon, if it is enabled, answers on its socket and fronts this transport's
    /// server. Probed once, on first use.
    pub fn daemon(&self) -> Option<&Link> {
        self.daemon
            .get_or_init(|| {
                if !self.daemon_enabled {
                    return None;
                }
                let token = self.token.as_ref().ok().cloned().flatten();
                Link::connect(
                    &daemon_glue::socket_path(None),
                    token.as_deref(),
                    self.endpoint.as_deref().ok(),
                )
            })
            .as_ref()
    }

    /// The endpoint text, if one could be chosen.
    pub fn endpoint(&self) -> Option<String> {
        self.endpoint.as_ref().ok().cloned()
    }

    /// Uploads `body` to a signed URL from the server (see [`Client::put`]).
    pub fn put(
        &self,
        target: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<(), Box<ErrorDetail>> {
        let (client, runtime) = self.get()?;
        runtime
            .block_on(client.put(target, content_type, body))
            .map_err(|e| detail_of(&e))
    }

    fn get(&self) -> Result<&(Client, tokio::runtime::Runtime), Box<ErrorDetail>> {
        let ready = self.ready.get_or_init(|| {
            let endpoint = Endpoint::parse(self.endpoint.as_ref().map_err(Clone::clone)?)?;
            let token = self.token.clone()?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| CliError::internal(format!("cannot start the I/O runtime: {e}")))?;
            Ok((Client::new(endpoint, token, self.timeout), runtime))
        });
        ready.as_ref().map_err(detail_of)
    }
}

impl bee_wc::Transport for CliTransport {
    fn unary(&self, path: &str, request: &Value) -> Result<Value, Box<ErrorDetail>> {
        let (client, runtime) = self.get()?;
        let body = serde_json::to_vec(request)
            .map_err(|e| detail_of(&CliError::internal(e.to_string())))?;
        let bytes = runtime
            .block_on(client.unary(path, body))
            .map_err(|e| detail_of(&e))?;
        parse(&bytes)
    }

    fn server_stream(&self, path: &str, request: &Value) -> Result<Vec<Value>, Box<ErrorDetail>> {
        if path == GET_OBJECTS
            && let Some(link) = self.daemon()
        {
            match link.server_stream(path, request) {
                Ok(items) => return Ok(items),
                // The daemon went away mid-run: the server can answer the same question.
                Err(e) if e.reason == "UNAVAILABLE" => daemon_glue::trace(&format!(
                    "daemon unavailable ({}); fetching direct",
                    e.message
                )),
                Err(e) => return Err(e),
            }
        }
        let (client, runtime) = self.get()?;
        let body = serde_json::to_vec(request)
            .map_err(|e| detail_of(&CliError::internal(e.to_string())))?;
        let mut out = Vec::new();
        let mut bad: Option<Box<ErrorDetail>> = None;
        let mut on = |bytes: &[u8]| {
            match parse(bytes) {
                Ok(v) => out.push(v),
                Err(d) => bad = Some(d),
            }
            Ok(())
        };
        runtime
            .block_on(client.stream(path, &[body], StreamMode::ToEnd, &mut on))
            .map_err(|e| detail_of(&e))?;
        match bad {
            Some(d) => Err(d),
            None => Ok(out),
        }
    }

    fn server_streams(
        &self,
        path: &str,
        requests: &[Value],
    ) -> Vec<Result<Vec<Value>, Box<ErrorDetail>>> {
        // Through the node daemon, calls go one at a time (it answers from its own cache).
        if path == GET_OBJECTS && self.daemon().is_some() {
            return requests
                .iter()
                .map(|r| self.server_stream(path, r))
                .collect();
        }
        let (client, runtime) = match self.get() {
            Ok(r) => r,
            Err(e) => return requests.iter().map(|_| Err(e.clone())).collect(),
        };
        let one = |request: &Value| {
            let body = serde_json::to_vec(request);
            async move {
                let body = body.map_err(|e| detail_of(&CliError::internal(e.to_string())))?;
                let mut out = Vec::new();
                let mut bad: Option<Box<ErrorDetail>> = None;
                let mut on = |bytes: &[u8]| {
                    match parse(bytes) {
                        Ok(v) => out.push(v),
                        Err(d) => bad = Some(d),
                    }
                    Ok(())
                };
                client
                    .stream(path, &[body], StreamMode::ToEnd, &mut on)
                    .await
                    .map_err(|e| detail_of(&e))?;
                match bad {
                    Some(d) => Err(d),
                    None => Ok(out),
                }
            }
        };
        runtime.block_on(buffered(requests.iter().map(one)))
    }

    fn get_objects(
        &self,
        requests: &[v1::GetObjectsRequest],
    ) -> Option<Vec<Result<Vec<v1::ObjectChunk>, Box<ErrorDetail>>>> {
        // The node daemon speaks proto3-JSON to working copies: use it through that path.
        if self.daemon().is_some() {
            return None;
        }
        let (client, runtime) = match self.get() {
            Ok(r) => r,
            Err(e) => return Some(requests.iter().map(|_| Err(e.clone())).collect()),
        };
        let one = |request: &v1::GetObjectsRequest| {
            let body = request.encode_to_vec();
            async move {
                let mut out = Vec::new();
                let mut bad: Option<Box<ErrorDetail>> = None;
                let mut on = |bytes: &[u8]| {
                    match v1::ObjectChunk::decode(bytes) {
                        Ok(c) => out.push(c),
                        Err(e) => {
                            bad = Some(detail_of(&CliError::internal(format!(
                                "server sent an invalid GetObjects message: {e}"
                            ))))
                        }
                    }
                    Ok(())
                };
                client
                    .stream_proto(GET_OBJECTS, &[body], StreamMode::ToEnd, &mut on)
                    .await
                    .map_err(|e| detail_of(&e))?;
                match bad {
                    Some(d) => Err(d),
                    None => Ok(out),
                }
            }
        };
        Some(runtime.block_on(buffered(requests.iter().map(one))))
    }

    fn list_tree(
        &self,
        request: &v1::ListTreeRequest,
    ) -> Option<Result<v1::ListTreeResponse, Box<ErrorDetail>>> {
        let (client, runtime) = match self.get() {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        let bytes = runtime
            .block_on(client.unary_proto("/bee.v1.QueryService/ListTree", request.encode_to_vec()))
            .map_err(|e| detail_of(&e));
        Some(bytes.and_then(|b| {
            v1::ListTreeResponse::decode(b.as_slice()).map_err(|e| {
                detail_of(&CliError::internal(format!(
                    "server sent an invalid ListTree response: {e}"
                )))
            })
        }))
    }

    fn client_stream(&self, path: &str, requests: &[Value]) -> Result<Value, Box<ErrorDetail>> {
        let (client, runtime) = self.get()?;
        let mut bodies = Vec::with_capacity(requests.len());
        for r in requests {
            bodies.push(
                serde_json::to_vec(r).map_err(|e| detail_of(&CliError::internal(e.to_string())))?,
            );
        }
        let mut last: Option<Vec<u8>> = None;
        let mut on = |bytes: &[u8]| {
            last = Some(bytes.to_vec());
            Ok(())
        };
        runtime
            .block_on(client.stream(path, &bodies, StreamMode::ToEnd, &mut on))
            .map_err(|e| detail_of(&e))?;
        parse(&last.unwrap_or_default())
    }
}

/// Runs `calls` up to [`CONCURRENT_STREAMS`] at once and returns their results in order.
async fn buffered<T>(calls: impl Iterator<Item = impl std::future::Future<Output = T>>) -> Vec<T> {
    use futures_util::StreamExt as _;
    futures_util::stream::iter(calls)
        .buffered(CONCURRENT_STREAMS)
        .collect()
        .await
}
