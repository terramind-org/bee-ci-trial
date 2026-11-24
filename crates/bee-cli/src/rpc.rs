//! A small Connect-protocol HTTP/JSON client (SPEC §12.1): unary calls and enveloped streams
//! over HTTP/1.1 on TCP (`http://host:port`) or a unix socket (`unix:/path/to/socket`).
//!
//! TLS is not built into this client: use `http://` on loopback or a private network, a unix
//! socket, or a local TLS-terminating proxy.

use std::path::PathBuf;
use std::time::Duration;

use bee_proto::connect::{ConnectError, EndStream};
use bee_proto::envelope::{self, FLAG_END_STREAM};
use bee_proto::v1::ErrorReason;
use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, USER_AGENT};
use http::{Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;

use crate::error::CliError;

/// Largest single message the client accepts: 64 MiB.
const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Where the server listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// `http://host:port[/prefix]`.
    Http {
        /// Host name or address.
        host: String,
        /// TCP port (80 when omitted).
        port: u16,
        /// Path prefix without a trailing slash, possibly empty.
        prefix: String,
    },
    /// `unix:/path` or `unix:///path`.
    Unix(PathBuf),
}

impl Endpoint {
    /// Parses an endpoint URL.
    pub fn parse(text: &str) -> Result<Self, CliError> {
        let text = text.trim();
        if let Some(path) = text.strip_prefix("unix:") {
            let path = path.strip_prefix("//").unwrap_or(path);
            if path.is_empty() {
                return Err(CliError::usage(
                    "bad_endpoint",
                    "unix: endpoint needs a path",
                ));
            }
            return Ok(Self::Unix(PathBuf::from(path)));
        }
        if text.starts_with("https://") {
            return Err(CliError::usage(
                "bad_endpoint",
                format!(
                    "{text}: this client speaks plain HTTP only; use http:// on loopback or a \
                     private network, a unix: socket, or a local TLS-terminating proxy"
                ),
            ));
        }
        let rest = text.strip_prefix("http://").ok_or_else(|| {
            CliError::usage(
                "bad_endpoint",
                format!("{text:?} is not an endpoint: expected http://host:port or unix:/path"),
            )
        })?;
        let (authority, prefix) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
            None => (rest, ""),
        };
        let bracketed_without_port = authority.starts_with('[') && authority.ends_with(']');
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !bracketed_without_port => {
                let port = port.parse::<u16>().map_err(|_| {
                    CliError::usage("bad_endpoint", format!("bad port in endpoint {text:?}"))
                })?;
                (host, port)
            }
            _ => (authority, 80),
        };
        if host.is_empty() {
            return Err(CliError::usage(
                "bad_endpoint",
                format!("no host in endpoint {text:?}"),
            ));
        }
        Ok(Self::Http {
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            port,
            prefix: prefix.to_owned(),
        })
    }

    fn host_header(&self) -> String {
        match self {
            Self::Http { host, port, .. } if host.contains(':') => format!("[{host}]:{port}"),
            Self::Http { host, port, .. } => format!("{host}:{port}"),
            Self::Unix(_) => "localhost".to_owned(),
        }
    }

    fn uri(&self, path: &str) -> String {
        match self {
            Self::Http { prefix, .. } => format!("{prefix}{path}"),
            Self::Unix(_) => path.to_owned(),
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http { prefix, .. } => write!(f, "http://{}{prefix}", self.host_header()),
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

/// How a server stream is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamMode {
    /// Read until the server ends the stream.
    ToEnd,
    /// Stop quietly once no message arrived for this long (catch-up on an event stream).
    UntilIdle(Duration),
}

/// A Connect client bound to one endpoint and credential.
#[derive(Clone, Debug)]
pub struct Client {
    endpoint: Endpoint,
    token: Option<String>,
    timeout: Option<Duration>,
}

impl Client {
    /// A client for `endpoint`, sending `token` as a bearer credential when set.
    pub fn new(endpoint: Endpoint, token: Option<String>, timeout: Option<Duration>) -> Self {
        Self {
            endpoint,
            token,
            timeout,
        }
    }

    /// The endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Calls a unary RPC with a proto3-JSON body and returns the response body.
    pub async fn unary(&self, path: &str, body: Vec<u8>) -> Result<Vec<u8>, CliError> {
        self.unary_as(path, "application/json", body).await
    }

    /// Calls a unary RPC with a binary protobuf body and returns the response body.
    pub async fn unary_proto(&self, path: &str, body: Vec<u8>) -> Result<Vec<u8>, CliError> {
        self.unary_as(path, "application/proto", body).await
    }

    async fn unary_as(
        &self,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, CliError> {
        self.deadline(async {
            let response = self.send(path, content_type, body).await?;
            let status = response.status();
            let bytes = collect(response.into_body()).await?;
            if status == StatusCode::OK {
                Ok(bytes.to_vec())
            } else {
                Err(error_body(status, &bytes))
            }
        })
        .await
    }

    /// Calls a streaming RPC: sends every request message as one envelope, then hands each
    /// response message to `on_message` as it arrives.
    pub async fn stream(
        &self,
        path: &str,
        messages: &[Vec<u8>],
        mode: StreamMode,
        on_message: &mut dyn FnMut(&[u8]) -> Result<(), CliError>,
    ) -> Result<(), CliError> {
        self.stream_as(path, "application/connect+json", messages, mode, on_message)
            .await
    }

    /// [`Client::stream`] with binary protobuf messages (the end-of-stream frame is still
    /// JSON, as Connect specifies).
    pub async fn stream_proto(
        &self,
        path: &str,
        messages: &[Vec<u8>],
        mode: StreamMode,
        on_message: &mut dyn FnMut(&[u8]) -> Result<(), CliError>,
    ) -> Result<(), CliError> {
        self.stream_as(
            path,
            "application/connect+proto",
            messages,
            mode,
            on_message,
        )
        .await
    }

    async fn stream_as(
        &self,
        path: &str,
        content_type: &str,
        messages: &[Vec<u8>],
        mode: StreamMode,
        on_message: &mut dyn FnMut(&[u8]) -> Result<(), CliError>,
    ) -> Result<(), CliError> {
        self.deadline(async {
            let mut body = Vec::new();
            for message in messages {
                body.extend_from_slice(&envelope::encode(0, message));
            }
            let response = self.send(path, content_type, body).await?;
            let status = response.status();
            let mut incoming = response.into_body();
            if status != StatusCode::OK {
                let bytes = collect(incoming).await?;
                return Err(error_body(status, &bytes));
            }
            let mut decoder = envelope::Decoder::new(MAX_MESSAGE_BYTES);
            loop {
                while let Some(frame) = decoder
                    .next_frame()
                    .map_err(|e| CliError::internal(format!("malformed stream: {e}")))?
                {
                    if frame.flags & FLAG_END_STREAM != 0 {
                        let end: EndStream = serde_json::from_slice(&frame.data).map_err(|e| {
                            CliError::internal(format!("malformed end-of-stream frame: {e}"))
                        })?;
                        return match end.error {
                            Some(error) => Err(CliError::from_connect(&error)),
                            None => Ok(()),
                        };
                    }
                    on_message(&frame.data)?;
                }
                let next = match mode {
                    StreamMode::ToEnd => incoming.frame().await,
                    StreamMode::UntilIdle(idle) => {
                        match tokio::time::timeout(idle, incoming.frame()).await {
                            Ok(next) => next,
                            Err(_) => return Ok(()),
                        }
                    }
                };
                match next {
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            decoder.push(&data);
                        }
                    }
                    Some(Err(e)) => {
                        return Err(CliError::unavailable(format!("stream interrupted: {e}")));
                    }
                    None => {
                        return Err(CliError::unavailable(
                            "stream ended before its end-of-stream frame",
                        ));
                    }
                }
            }
        })
        .await
    }

    /// Sends `body` with `PUT` to a signed upload URL the server handed out (SPEC §11.9). Only
    /// the path and query of `target` are used: the bytes go to this client's own endpoint, so
    /// a server that advertises its public address still works behind a local proxy or socket.
    /// No credential is sent; the signature in the query is the authority.
    pub async fn put(
        &self,
        target: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<(), CliError> {
        self.deadline(async {
            let path = match target.split_once("://") {
                Some((_, rest)) => rest.find('/').map(|i| &rest[i..]).unwrap_or("/"),
                None => target,
            };
            let mut sender = self.sender().await?;
            let request = Request::put(self.endpoint.uri(path))
                .header(HOST, self.endpoint.host_header())
                .header(CONTENT_TYPE, content_type)
                .header(USER_AGENT, concat!("bee-cli/", env!("CARGO_PKG_VERSION")))
                .body(Full::new(Bytes::from(body)))
                .map_err(|e| {
                    CliError::usage("bad_request", format!("cannot build request: {e}"))
                })?;
            let response = sender.send_request(request).await.map_err(|e| {
                CliError::unavailable(format!("cannot reach {}: {e}", self.endpoint))
            })?;
            let status = response.status();
            let bytes = collect(response.into_body()).await?;
            if status.is_success() {
                Ok(())
            } else {
                Err(error_body(status, &bytes))
            }
        })
        .await
    }

    async fn deadline<T>(
        &self,
        work: impl std::future::Future<Output = Result<T, CliError>>,
    ) -> Result<T, CliError> {
        match self.timeout {
            Some(limit) => tokio::time::timeout(limit, work).await.unwrap_or_else(|_| {
                Err(CliError::new(
                    ErrorReason::DeadlineExceeded,
                    format!("no complete response within {} ms", limit.as_millis()),
                ))
            }),
            None => work.await,
        }
    }

    /// Opens a connection to the endpoint.
    async fn sender(
        &self,
    ) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>, CliError> {
        let unreachable = |e: &dyn std::fmt::Display| {
            CliError::unavailable(format!("cannot reach {}: {e}", self.endpoint))
        };
        let sender = match &self.endpoint {
            Endpoint::Http { host, port, .. } => {
                let stream = tokio::net::TcpStream::connect((host.as_str(), *port))
                    .await
                    .map_err(|e| unreachable(&e))?;
                handshake(stream).await.map_err(|e| unreachable(&e))?
            }
            Endpoint::Unix(socket) => {
                #[cfg(unix)]
                {
                    let stream = tokio::net::UnixStream::connect(socket)
                        .await
                        .map_err(|e| unreachable(&e))?;
                    handshake(stream).await.map_err(|e| unreachable(&e))?
                }
                #[cfg(not(unix))]
                {
                    return Err(CliError::usage(
                        "bad_endpoint",
                        format!("unix sockets are not available here: {}", socket.display()),
                    ));
                }
            }
        };
        Ok(sender)
    }

    async fn send(
        &self,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<http::Response<Incoming>, CliError> {
        let mut sender = self.sender().await?;
        let unreachable = |e: &dyn std::fmt::Display| {
            CliError::unavailable(format!("cannot reach {}: {e}", self.endpoint))
        };
        let mut builder = Request::post(self.endpoint.uri(path))
            .header(HOST, self.endpoint.host_header())
            .header(CONTENT_TYPE, content_type)
            .header("connect-protocol-version", "1")
            .header(USER_AGENT, concat!("bee-cli/", env!("CARGO_PKG_VERSION")));
        if let Some(token) = &self.token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(limit) = self.timeout {
            builder = builder.header("connect-timeout-ms", limit.as_millis().to_string());
        }
        let request = builder
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| CliError::usage("bad_request", format!("cannot build request: {e}")))?;
        sender
            .send_request(request)
            .await
            .map_err(|e| unreachable(&e))
    }
}

async fn handshake<S>(
    stream: S,
) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>, hyper::Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        // The connection ends with the call; its own error surfaces on the response future.
        let _ = connection.await;
    });
    Ok(sender)
}

async fn collect(body: Incoming) -> Result<Bytes, CliError> {
    body.collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| CliError::unavailable(format!("response interrupted: {e}")))
}

fn error_body(status: StatusCode, body: &[u8]) -> CliError {
    match serde_json::from_slice::<ConnectError>(body) {
        Ok(error) => CliError::from_connect(&error),
        Err(_) => CliError::from_http_status(status.as_u16(), &String::from_utf8_lossy(body)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_parse() {
        assert_eq!(
            Endpoint::parse("http://127.0.0.1:8080").expect("tcp"),
            Endpoint::Http {
                host: "127.0.0.1".into(),
                port: 8080,
                prefix: String::new()
            }
        );
        assert_eq!(
            Endpoint::parse("http://example.test/api/").expect("prefix"),
            Endpoint::Http {
                host: "example.test".into(),
                port: 80,
                prefix: "/api".into()
            }
        );
        assert_eq!(
            Endpoint::parse("http://[::1]:9000")
                .expect("v6")
                .to_string(),
            "http://[::1]:9000"
        );
        assert_eq!(
            Endpoint::parse("http://[::1]")
                .expect("v6 default port")
                .to_string(),
            "http://[::1]:80"
        );
        assert_eq!(
            Endpoint::parse("unix:///tmp/bee.sock").expect("unix"),
            Endpoint::Unix(PathBuf::from("/tmp/bee.sock"))
        );
        for bad in [
            "ftp://x",
            "http://",
            "http://h:notaport",
            "https://x",
            "unix:",
        ] {
            let error = Endpoint::parse(bad).expect_err(bad);
            assert_eq!(error.exit.code(), 2, "{bad}");
        }
    }
}
