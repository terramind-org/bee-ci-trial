//! GitHub REST and GraphQL compatibility gateway over the Beehive forge
//! (`docs/spec/github-compat.md`).
//!
//! Unmodified GitHub clients (`gh`, Octokit, CI integrations, `GITHUB_TOKEN` automation)
//! talk to Beehive through this crate. It is a translator: every request is authenticated
//! with the bee token behind the presented credential ([`ports::TokenMap`]), turned into
//! the forge's own RPCs (so authorization, policy, audit and events are exactly those of the
//! native API) and answered in GitHub's JSON shapes. Git objects come from a
//! [`ports::GitPort`] in git ids.
//!
//! # Mounting
//!
//! ```text
//! /api/v3/...        REST (GitHub Enterprise Server layout)        Compat::router
//! /api/graphql       GraphQL                                       Compat::router
//! /api/uploads/...   release asset uploads                         Compat::router
//! /user, /repos/...  root aliases (`api.<host>/` layout gh uses)   Compat::root_router
//! /graphql
//! ```
//!
//! # Layout
//!
//! | module | owns |
//! |---|---|
//! | [`hub`] | [`Compat`], its builder, settings and the rate limiter |
//! | [`ports`] | the traits the server wires |
//! | [`memory`] | in-process port implementations over the bee stores |
//! | `http` | routing, credentials, rate limits, API versions, ETags, pagination |
//! | `rest` | the REST endpoint groups |
//! | `graphql` | the query parser, executor, schema and resolvers |
//! | `shapes` | GitHub's JSON objects |
//! | [`sealed`] | libsodium sealed boxes for Actions secrets |
//! | [`error`] | GitHub's error body with the SPEC §12.2 reason |

#![recursion_limit = "512"]

pub mod error;
mod graphql;
mod http;
pub mod hub;
pub mod ids;
pub mod memory;
pub mod ports;
mod rest;
pub mod sealed;
mod shapes;
pub mod time;

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::routing::{any, post};

pub use error::{ApiError, ApiResult};
pub use hub::{API_VERSION, Compat, CompatBuilder, CompatConfig, Ports};

/// Top-level path segments served at the root for the `api.<host>/` layout.
const ROOT_SEGMENTS: &[&str] = &[
    "user",
    "users",
    "repos",
    "orgs",
    "organizations",
    "search",
    "gists",
    "notifications",
    "rate_limit",
    "meta",
    "teams",
    "installation",
    "emojis",
    "octocat",
    "zen",
    "licenses",
    "markdown",
    "feeds",
    "events",
    "repositories",
];

impl Compat {
    /// REST under `/api/v3`, GraphQL at `/api/graphql` and uploads under `/api/uploads`.
    pub fn router(self: &Arc<Self>) -> Router {
        let rest = {
            let hub = self.clone();
            move |req: Request| {
                let hub = hub.clone();
                async move { http::dispatch(hub, "/api/v3", req).await }
            }
        };
        let gql = {
            let hub = self.clone();
            move |req: Request| {
                let hub = hub.clone();
                async move { graphql::handle(hub, "/api", req).await }
            }
        };
        let up = {
            let hub = self.clone();
            move |req: Request| {
                let hub = hub.clone();
                async move { rest::releases::upload(hub, req).await }
            }
        };
        Router::new()
            .route("/api/v3", any(rest.clone()))
            .route("/api/v3/", any(rest.clone()))
            .route("/api/v3/{*rest}", any(rest))
            .route("/api/graphql", post(gql))
            .route("/api/uploads/{*rest}", any(up))
    }

    /// The same REST and GraphQL at the root (`/user`, `/repos/...`, `/graphql`): the
    /// layout `gh` and Octokit use for `api.<host>`. Only the top-level segments GitHub's
    /// API uses are claimed, so the router merges with the web UI.
    pub fn root_router(self: &Arc<Self>) -> Router {
        let rest = {
            let hub = self.clone();
            move |req: Request| {
                let hub = hub.clone();
                async move { http::dispatch(hub, "", req).await }
            }
        };
        let gql = {
            let hub = self.clone();
            move |req: Request| {
                let hub = hub.clone();
                async move { graphql::handle(hub, "", req).await }
            }
        };
        let mut r = Router::new().route("/graphql", post(gql));
        for seg in ROOT_SEGMENTS {
            r = r
                .route(&format!("/{seg}"), any(rest.clone()))
                .route(&format!("/{seg}/{{*rest}}"), any(rest.clone()));
        }
        r
    }
}
