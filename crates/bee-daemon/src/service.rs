//! `bee.v1.ObjectService` served by a cache tier: the same RPCs, request and response
//! messages as the server (tier conformance), answered from [`crate::tier::Tier`].

use std::pin::Pin;
use std::sync::Arc;

use bee_proto::v1::{self, object_service_server::ObjectService};
use futures_util::Stream;

use crate::error::to_status;
use crate::tier::Tier;
use crate::upstream::{Auth, Epochs};

/// The tier's `ObjectService`.
#[derive(Clone)]
pub struct TierService {
    tier: Arc<Tier>,
}

impl TierService {
    /// A service over `tier`.
    pub fn new(tier: Arc<Tier>) -> Self {
        Self { tier }
    }
}

/// The credential of a request (the `authorization` metadata), forwarded unchanged.
pub fn auth_of<T>(req: &tonic::Request<T>) -> Auth {
    Auth(
        req.metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    )
}

fn respond<T>(msg: T, epochs: Option<&Epochs>, tier: &Tier) -> tonic::Response<T> {
    let mut resp = tonic::Response::new(msg);
    let md = resp.metadata_mut();
    if let Some(e) = epochs {
        if let Ok(v) = e.policy.parse() {
            md.insert("bee-policy-epoch", v);
        }
        if let Ok(v) = e.redaction.parse() {
            md.insert("bee-redaction-epoch", v);
        }
    }
    if let Ok(v) = tier.config().kind.stats_name().parse() {
        md.insert("bee-tier", v);
    }
    resp
}

type Rpc<T> = Result<tonic::Response<T>, tonic::Status>;

/// The stream type of `GetObjects`.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<v1::ObjectChunk, tonic::Status>> + Send>>;

#[tonic::async_trait]
impl ObjectService for TierService {
    async fn get_server_info(
        &self,
        req: tonic::Request<v1::GetServerInfoRequest>,
    ) -> Rpc<v1::ServerInfo> {
        if req.get_ref().repo.is_some() {
            let auth = auth_of(&req);
            return match self.tier.repo_server_info(&auth, req.into_inner()).await {
                Ok((info, epochs)) => Ok(respond(info, epochs.as_ref(), &self.tier)),
                Err(e) => Err(to_status(&e)),
            };
        }
        match self.tier.server_info().await {
            Ok(info) => Ok(respond(info, None, &self.tier)),
            Err(e) => Err(to_status(&e)),
        }
    }

    type GetObjectsStream = ChunkStream;

    async fn get_objects(
        &self,
        req: tonic::Request<v1::GetObjectsRequest>,
    ) -> Rpc<Self::GetObjectsStream> {
        let auth = auth_of(&req);
        match self.tier.get_objects(&auth, req.into_inner()).await {
            Ok((chunks, epochs)) => {
                let stream: ChunkStream =
                    Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok)));
                Ok(respond(stream, epochs.as_ref(), &self.tier))
            }
            Err(e) => Err(to_status(&e)),
        }
    }

    async fn has_objects(
        &self,
        req: tonic::Request<v1::HasObjectsRequest>,
    ) -> Rpc<v1::HasObjectsResponse> {
        let auth = auth_of(&req);
        match self.tier.has_objects(&auth, req.into_inner()).await {
            Ok((resp, epochs)) => Ok(respond(resp, epochs.as_ref(), &self.tier)),
            Err(e) => Err(to_status(&e)),
        }
    }

    async fn resolve_ref(
        &self,
        req: tonic::Request<v1::ResolveRefRequest>,
    ) -> Rpc<v1::ResolveRefResponse> {
        let auth = auth_of(&req);
        match self.tier.resolve_ref(&auth, req.into_inner()).await {
            Ok((resp, epochs)) => Ok(respond(resp, epochs.as_ref(), &self.tier)),
            Err(e) => Err(to_status(&e)),
        }
    }

    async fn get_tier_stats(
        &self,
        _req: tonic::Request<v1::GetTierStatsRequest>,
    ) -> Rpc<v1::TierStats> {
        Ok(respond(self.tier.tier_stats(), None, &self.tier))
    }

    async fn get_daemon_snapshot(
        &self,
        req: tonic::Request<v1::GetDaemonSnapshotRequest>,
    ) -> Rpc<v1::DaemonSnapshot> {
        let auth = auth_of(&req);
        match self
            .tier
            .upstream()
            .daemon_snapshot(&auth, req.into_inner())
            .await
        {
            Ok(s) => Ok(respond(s, None, &self.tier)),
            Err(e) => Err(to_status(&e)),
        }
    }
}
