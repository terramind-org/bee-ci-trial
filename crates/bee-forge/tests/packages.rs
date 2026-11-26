//! Packages: the store on every backend, the `PackageService` permission matrix, and each
//! registry protocol driven through the transport-agnostic handlers
//! (`PackageRegistry::handle`). Protocol tests are written from the public specifications.
//!
//! Opt-in real-client tests (`npm`, `docker`/`oras`, `cargo`) are in `packages/client.rs` and
//! run only with `BEE_TEST_CLIENTS=1` and a server that mounts the handlers (see
//! `docs/spec/packages-pages.md`, "Real clients").

use crate::packages_support as support;

#[path = "packages/cargo.rs"]
mod cargo;
#[path = "packages/client.rs"]
mod client;
#[path = "packages/maven.rs"]
mod maven;
#[path = "packages/npm.rs"]
mod npm;
#[path = "packages/nuget.rs"]
mod nuget;
#[path = "packages/oci.rs"]
mod oci;
#[path = "packages/rubygems.rs"]
mod rubygems;
#[path = "packages/service.rs"]
mod service;
#[path = "packages/store.rs"]
mod store;
