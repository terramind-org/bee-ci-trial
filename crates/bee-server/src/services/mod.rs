//! The service registry: every tonic service the server answers, mounted once and served on
//! both surfaces (gRPC and the Connect HTTP/JSON gateway) behind the request layer.
//!
//! # Plugging in a forge area
//!
//! A forge crate implements the generated trait (for example
//! `bee_proto::v1::lease_service_server::LeaseService`) and registers the tonic server:
//!
//! ```ignore
//! registry.add(LeaseServiceServer::new(MyLeases::new(state.clone())));
//! ```
//!
//! The server calls [`ServiceRegistry::add`] for its own VCS services in
//! [`ServiceRegistry::with_core`] and then passes the registry to the `extra` hook of
//! [`crate::server::ServerOptions`], where forge areas add theirs. Any service that is not
//! registered answers `UNIMPLEMENTED` on gRPC (tonic's router fallback) and on HTTP/JSON
//! (the gateway's unknown-service answer), with a `bee.v1.ErrorDetail`.

use std::convert::Infallible;
use std::sync::Arc;

use bee_proto::connect::ConnectGateway;
use bee_proto::v1;
use bytes::Bytes;
use http_body_util::Full;
use tonic::server::NamedService;
use tower::Service;

use crate::layer::Layered;
use crate::state::AppState;

pub mod admin;
pub mod audit_sinks;
pub mod auth;
pub mod daemon;
pub mod export;
pub mod import;
pub mod object;
pub mod ops;
pub mod policy;
pub mod query;
pub mod refs;
pub mod search;
pub mod second_factor;
pub mod support;
pub mod vcs_gateway;
pub mod write;

/// Background workers of the services in this module (audit sink delivery when the forge
/// runs); the server aborts them on shutdown.
pub fn spawn_workers(
    state: &Arc<AppState>,
    forge: Option<&Arc<crate::forge::ForgeWiring>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut out = vec![ops::spawn_sampler(state.clone())];
    if let Some(w) = forge {
        out.push(audit_sinks::spawn(state.clone(), w.clone()));
    }
    out
}

/// Every service mounted on this server, on both surfaces.
pub struct ServiceRegistry {
    state: Arc<AppState>,
    routes: tonic::service::Routes,
    gateway: ConnectGateway,
    names: Vec<&'static str>,
}

impl ServiceRegistry {
    /// An empty registry.
    pub fn new(state: Arc<AppState>) -> ServiceRegistry {
        let gateway = ConnectGateway::new().max_message_bytes(state.cfg.limits.max_message_bytes);
        ServiceRegistry {
            state,
            routes: tonic::service::Routes::default(),
            gateway,
            names: Vec::new(),
        }
    }

    /// The shared state, for services that need it.
    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    /// Registers a tonic server on gRPC and HTTP/JSON, behind the request layer.
    pub fn add<S>(&mut self, svc: S) -> &mut Self
    where
        S: Service<
                http::Request<tonic::body::Body>,
                Response = http::Response<tonic::body::Body>,
                Error = Infallible,
            > + Service<
                http::Request<Full<Bytes>>,
                Response = http::Response<tonic::body::Body>,
                Error = Infallible,
            > + NamedService
            + Clone
            + Send
            + Sync
            + 'static,
        <S as Service<http::Request<tonic::body::Body>>>::Future: Send + 'static,
        <S as Service<http::Request<Full<Bytes>>>>::Future: Send + 'static,
    {
        let layered = Layered::new(svc, self.state.clone());
        let routes = std::mem::take(&mut self.routes);
        self.routes = routes.add_service(layered.clone());
        let gateway = std::mem::take(&mut self.gateway);
        self.gateway = gateway.add_service(layered);
        self.names.push(S::NAME);
        self
    }

    /// Registers the VCS core services this crate implements.
    pub fn with_core(mut self) -> ServiceRegistry {
        let s = self.state.clone();
        self.add(v1::object_service_server::ObjectServiceServer::new(
            object::Objects::new(s.clone()),
        ))
        .add(v1::query_service_server::QueryServiceServer::new(
            query::Queries::new(s.clone()),
        ))
        .add(
            v1::write_service_server::WriteServiceServer::new(write::Writes::new(s.clone()))
                .max_decoding_message_size(s.cfg.limits.max_message_bytes),
        )
        .add(v1::ref_service_server::RefServiceServer::new(
            refs::Refs::new(s.clone()),
        ))
        .add(v1::auth_service_server::AuthServiceServer::new(
            auth::AuthApi::new(s.clone()),
        ))
        .add(v1::admin_service_server::AdminServiceServer::new(
            admin::Admin::new(s.clone()),
        ))
        .add(v1::import_service_server::ImportServiceServer::new(
            import::Imports::new(s.clone()),
        ))
        .add(v1::search_service_server::SearchServiceServer::new(
            search::SearchApi::new(s.clone()),
        ))
        .add(v1::policy_service_server::PolicyServiceServer::new(
            policy::PolicyApi::new(s.clone()),
        ))
        .add(v1::ops_service_server::OpsServiceServer::new(
            ops::OpsApi::new(s.clone()),
        ))
        .add(v1::git_service_server::GitServiceServer::new(
            vcs_gateway::GitApi::new(s),
        ));
        self
    }

    /// Names of the registered services.
    pub fn names(&self) -> &[&'static str] {
        &self.names
    }

    /// The gRPC router and the HTTP/JSON gateway router.
    pub fn into_routers(self) -> (axum::Router, axum::Router) {
        (
            self.routes.prepare().into_axum_router(),
            self.gateway.into_router(),
        )
    }
}

/// Shorthand for a gRPC response.
pub type Rpc<T> = Result<tonic::Response<T>, tonic::Status>;

/// A boxed response stream.
pub type RpcStream<T> =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<T, tonic::Status>> + Send + 'static>>;

/// A stream of already computed items.
pub fn stream_of<T: Send + 'static>(items: Vec<Result<T, tonic::Status>>) -> RpcStream<T> {
    Box::pin(futures_util::stream::iter(items))
}
