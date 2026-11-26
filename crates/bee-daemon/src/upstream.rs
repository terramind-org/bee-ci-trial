//! The tier directly above (PRODUCT §4.5: a tier talks only to the tier above it): the server,
//! or a regional cache in front of it.

use std::time::Duration;

use axum::body::Body;
use bee_proto::v1::{self, ErrorDetail};
use bytes::Bytes;
use http::{Request, Response};

use crate::client::{ConnectClient, Endpoint, Trailers};
use crate::error::Result;

/// The caller's credential, forwarded unchanged to the tier above (auth passthrough).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Auth(pub Option<String>);

impl Auth {
    /// The `Authorization` header value.
    pub fn header(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// A stable key for caching decisions made for this credential: BLAKE3 of the header,
    /// so the daemon never keeps the credential itself.
    pub fn key(&self) -> AuthKey {
        match &self.0 {
            None => AuthKey([0u8; 32]),
            Some(h) => AuthKey(bee_object::digest(h.as_bytes())),
        }
    }
}

/// Digest of a credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AuthKey(pub [u8; 32]);

/// Policy and redaction epochs a response reported (SPEC §12.1 trailers).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Epochs {
    /// `bee-policy-epoch`.
    pub policy: String,
    /// `bee-redaction-epoch`.
    pub redaction: String,
}

impl Epochs {
    /// The epochs in `trailers`, if the response carried them.
    pub fn from_trailers(t: &Trailers) -> Option<Self> {
        let policy = t.get("bee-policy-epoch")?;
        let redaction = t.get("bee-redaction-epoch").unwrap_or("0");
        Some(Self {
            policy: policy.to_owned(),
            redaction: redaction.to_owned(),
        })
    }
}

/// A response from the tier above, with the epochs it reported.
#[derive(Clone, Debug, Default)]
pub struct Reply<T> {
    /// The message.
    pub msg: T,
    /// Epochs from the response trailers.
    pub epochs: Option<Epochs>,
}

/// The calls a cache tier makes on the tier above.
#[tonic::async_trait]
pub trait Upstream: Send + Sync + 'static {
    /// `ObjectService/GetObjects`, collected.
    async fn get_objects(
        &self,
        auth: &Auth,
        req: v1::GetObjectsRequest,
    ) -> Result<Reply<Vec<v1::ObjectChunk>>>;
    /// `ObjectService/HasObjects`.
    async fn has_objects(
        &self,
        auth: &Auth,
        req: v1::HasObjectsRequest,
    ) -> Result<Reply<v1::HasObjectsResponse>>;
    /// `ObjectService/ResolveRef`.
    async fn resolve_ref(
        &self,
        auth: &Auth,
        req: v1::ResolveRefRequest,
    ) -> Result<Reply<v1::ResolveRefResponse>>;
    /// `ObjectService/GetServerInfo`.
    async fn server_info(&self) -> Result<v1::ServerInfo>;
    /// `ObjectService/GetServerInfo` naming a repo, with the caller's credential: the answer
    /// carries that repo's epochs. The default answers `UNIMPLEMENTED`.
    async fn repo_server_info(
        &self,
        auth: &Auth,
        req: v1::GetServerInfoRequest,
    ) -> Result<Reply<v1::ServerInfo>> {
        let _ = (auth, req);
        Err(crate::error::err(
            v1::ErrorReason::Unimplemented,
            "this tier has no upstream for repo-scoped server info",
        ))
    }
    /// `ObjectService/GetDaemonSnapshot`.
    async fn daemon_snapshot(
        &self,
        auth: &Auth,
        req: v1::GetDaemonSnapshotRequest,
    ) -> Result<v1::DaemonSnapshot>;
    /// `QueryService/ListTree`.
    async fn list_tree(
        &self,
        auth: &Auth,
        req: v1::ListTreeRequest,
    ) -> Result<Reply<v1::ListTreeResponse>>;
    /// `QueryService/Resolve`.
    async fn resolve(
        &self,
        auth: &Auth,
        req: v1::ResolveRequest,
    ) -> Result<Reply<v1::ResolveResponse>>;
    /// Downloads a signed URL (SPEC §10.5).
    async fn fetch_url(&self, url: &str) -> std::result::Result<Bytes, String>;
    /// Forwards any other request byte for byte. The default answers `UNIMPLEMENTED`.
    async fn forward(&self, req: Request<Body>) -> Response<Body> {
        drop(req);
        crate::server::error_response(&crate::error::err(
            v1::ErrorReason::Unimplemented,
            "this tier has no upstream to forward to",
        ))
    }
}

/// The tier above, reached over the Connect protocol.
#[derive(Clone, Debug)]
pub struct HttpUpstream {
    client: ConnectClient,
}

impl HttpUpstream {
    /// An upstream at `url` (`http(s)://host:port`).
    pub fn new(url: &str, timeout: Duration) -> Result<Self> {
        let endpoint = Endpoint::parse(url)?;
        Ok(Self {
            client: ConnectClient::new(endpoint, timeout),
        })
    }

    /// The client.
    pub fn client(&self) -> &ConnectClient {
        &self.client
    }

    async fn unary<Req: prost::Message, Resp: prost::Message + Default>(
        &self,
        path: &str,
        auth: &Auth,
        req: &Req,
    ) -> Result<Reply<Resp>> {
        let (msg, t) = self.client.unary(path, auth.header(), req).await?;
        Ok(Reply {
            msg,
            epochs: Epochs::from_trailers(&t),
        })
    }
}

#[tonic::async_trait]
impl Upstream for HttpUpstream {
    async fn get_objects(
        &self,
        auth: &Auth,
        req: v1::GetObjectsRequest,
    ) -> Result<Reply<Vec<v1::ObjectChunk>>> {
        let (msg, t) = self
            .client
            .server_stream("/bee.v1.ObjectService/GetObjects", auth.header(), &req)
            .await?;
        Ok(Reply {
            msg,
            epochs: Epochs::from_trailers(&t),
        })
    }

    async fn has_objects(
        &self,
        auth: &Auth,
        req: v1::HasObjectsRequest,
    ) -> Result<Reply<v1::HasObjectsResponse>> {
        self.unary("/bee.v1.ObjectService/HasObjects", auth, &req)
            .await
    }

    async fn resolve_ref(
        &self,
        auth: &Auth,
        req: v1::ResolveRefRequest,
    ) -> Result<Reply<v1::ResolveRefResponse>> {
        self.unary("/bee.v1.ObjectService/ResolveRef", auth, &req)
            .await
    }

    async fn server_info(&self) -> Result<v1::ServerInfo> {
        Ok(self
            .unary(
                "/bee.v1.ObjectService/GetServerInfo",
                &Auth::default(),
                &v1::GetServerInfoRequest::default(),
            )
            .await?
            .msg)
    }

    async fn repo_server_info(
        &self,
        auth: &Auth,
        req: v1::GetServerInfoRequest,
    ) -> Result<Reply<v1::ServerInfo>> {
        self.unary("/bee.v1.ObjectService/GetServerInfo", auth, &req)
            .await
    }

    async fn daemon_snapshot(
        &self,
        auth: &Auth,
        req: v1::GetDaemonSnapshotRequest,
    ) -> Result<v1::DaemonSnapshot> {
        Ok(self
            .unary("/bee.v1.ObjectService/GetDaemonSnapshot", auth, &req)
            .await?
            .msg)
    }

    async fn list_tree(
        &self,
        auth: &Auth,
        req: v1::ListTreeRequest,
    ) -> Result<Reply<v1::ListTreeResponse>> {
        self.unary("/bee.v1.QueryService/ListTree", auth, &req)
            .await
    }

    async fn resolve(
        &self,
        auth: &Auth,
        req: v1::ResolveRequest,
    ) -> Result<Reply<v1::ResolveResponse>> {
        self.unary("/bee.v1.QueryService/Resolve", auth, &req).await
    }

    async fn fetch_url(&self, url: &str) -> std::result::Result<Bytes, String> {
        self.client.get_url(url).await
    }

    async fn forward(&self, req: Request<Body>) -> Response<Body> {
        let (mut parts, body) = req.into_parts();
        for h in [
            "host",
            "connection",
            "keep-alive",
            "transfer-encoding",
            "upgrade",
        ] {
            parts.headers.remove(h);
        }
        let req = Request::from_parts(parts, body);
        match self.client.send(req).await {
            Ok(resp) => {
                let (mut parts, body) = resp.into_parts();
                for h in ["connection", "keep-alive", "transfer-encoding"] {
                    parts.headers.remove(h);
                }
                Response::from_parts(parts, Body::new(body))
            }
            Err(e) => crate::server::error_response(&e),
        }
    }
}

/// Retries `call` while it fails with a retryable reason, at most `retries` more times, with
/// exponential backoff from 20 ms (SPEC §15.5: clients retry only retryable reasons).
pub async fn with_retries<T, F, Fut>(retries: u32, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        match call().await {
            Ok(v) => return Ok(v),
            Err(e) if e.retryable && attempt < retries => {
                let wait = if e.retry_after_ms > 0 {
                    e.retry_after_ms.min(2_000)
                } else {
                    20u64 << attempt
                };
                tokio::time::sleep(Duration::from_millis(wait)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// True if `e` says the tier above could not be reached or did not answer in time.
pub fn is_unreachable(e: &ErrorDetail) -> bool {
    matches!(e.reason.as_str(), "UNAVAILABLE" | "DEADLINE_EXCEEDED")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn retries_only_retryable_reasons_three_times() {
        let n = AtomicU32::new(0);
        let r: Result<()> = with_retries(3, || async {
            n.fetch_add(1, Ordering::SeqCst);
            Err(crate::error::unavailable("down"))
        })
        .await;
        assert!(r.is_err());
        assert_eq!(n.load(Ordering::SeqCst), 4, "one try plus three retries");
        let n = AtomicU32::new(0);
        let r: Result<()> = with_retries(3, || async {
            n.fetch_add(1, Ordering::SeqCst);
            Err(crate::error::err(v1::ErrorReason::PermissionDenied, "no"))
        })
        .await;
        assert!(r.is_err());
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn auth_keys_hide_the_credential() {
        let a = Auth(Some("Bearer secret".into()));
        assert_ne!(a.key(), Auth::default().key());
        assert_eq!(a.key(), Auth(Some("Bearer secret".into())).key());
    }
}
