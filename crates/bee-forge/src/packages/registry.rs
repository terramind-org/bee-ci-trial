//! The HTTP front of every registry, as a pure request-to-response function.
//!
//! [`PackageRegistry::handle`] takes an `http::Request<Bytes>` and returns an
//! `http::Response<Bytes>`; it knows nothing about sockets, TLS or the server framework. The
//! server mounts it under the prefixes in [`super::mounts`] and passes every request through.
//!
//! # Authentication
//!
//! Each client sends its token the way its ecosystem does (bearer, HTTP Basic password,
//! `X-NuGet-ApiKey`, a bare `Authorization` value); [`credential`](super::http) extracts it and
//! the forge authenticator verifies it exactly as for gRPC. Anonymous requests may read public
//! packages. For a package that is missing or not public, an anonymous request is answered
//! `401` rather than `404`, so existence of private packages is not revealed and clients
//! (`docker`, `npm`, `cargo`) are told to authenticate.
//!
//! # Permissions
//!
//! Reads need `read`, publishing needs `commit`, both on the package (see [`super::access`]);
//! creating a package needs ownership, org admin, or write on the repo it links to.

use std::sync::Arc;

use bee_proto::v1::{ErrorReason, PackageVisibility, Repo, RepoRef, repo_ref};
use bytes::Bytes;
use http::{Request, Response};

use super::access::Need;
use super::http::{Req, credential, header, resp, segments, tonic_request};
use super::manage::{Owner, Packages};
use super::model::{Eco, PackageRec};
use super::ports::ProvenanceClaim;
use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;

/// Options for creating a package on first publish.
#[derive(Clone, Debug, Default)]
pub(crate) struct CreateOpts {
    /// Requested visibility (`None` = inherit from the linked repo, else private).
    pub visibility: Option<PackageVisibility>,
    /// `org/name` of the repo the package declares as its source.
    pub link: Option<String>,
    /// Description.
    pub description: String,
}

/// The registries over a forge.
#[derive(Clone)]
pub struct PackageRegistry {
    forge: Arc<Forge>,
}

impl PackageRegistry {
    /// A registry front over `forge`.
    pub fn new(forge: Arc<Forge>) -> PackageRegistry {
        PackageRegistry { forge }
    }

    /// The forge.
    pub fn forge(&self) -> &Arc<Forge> {
        &self.forge
    }

    pub(crate) fn pk(&self) -> &Packages {
        &self.forge.packages
    }

    /// Serves one request. Unknown paths are `404`.
    pub async fn handle(&self, req: Request<Bytes>) -> Response<Bytes> {
        let path = req.uri().path().to_string();
        if path == "/v2" || path.starts_with("/v2/") {
            super::oci::handle(self, req).await
        } else if path.starts_with("/npm/") {
            super::npm::handle(self, req).await
        } else if path.starts_with("/maven/") {
            super::maven::handle(self, req).await
        } else if path.starts_with("/nuget/") {
            super::nuget::handle(self, req).await
        } else if path.starts_with("/rubygems/") {
            super::rubygems::handle(self, req).await
        } else if path.starts_with("/cargo/") {
            super::cargo::handle(self, req).await
        } else {
            resp(
                404,
                &[("content-type", "text/plain")],
                "not a registry path",
            )
        }
    }

    /// The caller of a request: `None` without a credential, an error for one that does not
    /// verify.
    pub(crate) async fn caller(&self, req: &Req) -> Result<Option<Caller>> {
        let Some(token) = credential(req) else {
            return Ok(None);
        };
        let r = tonic_request(req, &token)?;
        self.forge.authn.authenticate(&r).map(Some)
    }

    /// A caller that must exist (publishing and other writes).
    pub(crate) async fn require_caller(&self, req: &Req) -> Result<Caller> {
        self.caller(req).await?.ok_or_else(|| {
            ForgeError::new(
                ErrorReason::Unauthenticated,
                "this request needs a credential",
            )
        })
    }

    pub(crate) fn lookup_cx(&self) -> Cx {
        Cx::system()
    }

    /// Resolves the owner and the live package for a read, authorizing `Need::Read`.
    pub(crate) async fn read_pkg(
        &self,
        caller: Option<&Caller>,
        owner_name: &str,
        eco: Eco,
        name: &str,
    ) -> Result<(Owner, PackageRec, Cx)> {
        self.pkg_for(caller, owner_name, eco, name, Need::Read)
            .await
    }

    /// Resolves and authorizes `need` on an existing package.
    pub(crate) async fn pkg_for(
        &self,
        caller: Option<&Caller>,
        owner_name: &str,
        eco: Eco,
        name: &str,
        need: Need,
    ) -> Result<(Owner, PackageRec, Cx)> {
        let lookup = self.lookup_cx();
        let hidden = |e: ForgeError| {
            if caller.is_none() {
                ForgeError::new(
                    ErrorReason::Unauthenticated,
                    "this package needs a credential",
                )
            } else {
                e
            }
        };
        let owner = self.pk().owner(&lookup, owner_name).await.map_err(hidden)?;
        let p = match self.pk().find(&lookup, &owner, eco, name).await? {
            Some(p) => p,
            None => {
                return Err(hidden(ForgeError::not_found(format!(
                    "package {owner_name}/{name}"
                ))));
            }
        };
        let cx = self.pk().authorize(caller, &p, need).await.map_err(|e| {
            if caller.is_none() {
                ForgeError::new(
                    ErrorReason::Unauthenticated,
                    "this package needs a credential",
                )
            } else {
                e
            }
        })?;
        Ok((owner, p, cx))
    }

    /// Resolves the package a publish targets: authorizes `Write` on an existing one, or
    /// authorizes creation and creates it. The bool is whether it was created.
    pub(crate) async fn publish_target(
        &self,
        caller: &Caller,
        owner_name: &str,
        eco: Eco,
        name: &str,
        opts: &CreateOpts,
    ) -> Result<(Owner, PackageRec, Cx, bool)> {
        let lookup = self.lookup_cx();
        let owner = self.pk().owner(&lookup, owner_name).await?;
        if let Some(p) = self.pk().find(&lookup, &owner, eco, name).await? {
            let cx = self.pk().authorize(Some(caller), &p, Need::Write).await?;
            return Ok((owner, p, cx, false));
        }
        let repo = match &opts.link {
            Some(full) => self.resolve_repo(full).await,
            None => None,
        };
        let cx = self
            .pk()
            .authorize_create(caller, &owner, repo.as_ref())
            .await?;
        let (visibility, repo_id) = match (&repo, opts.visibility) {
            (Some(r), v) => (
                v.unwrap_or(
                    match bee_proto::v1::Visibility::try_from(r.visibility)
                        .unwrap_or(bee_proto::v1::Visibility::Private)
                    {
                        bee_proto::v1::Visibility::Public => PackageVisibility::Public,
                        bee_proto::v1::Visibility::Internal => PackageVisibility::Internal,
                        _ => PackageVisibility::Private,
                    },
                ),
                r.id.clone(),
            ),
            (None, v) => (v.unwrap_or(PackageVisibility::Private), String::new()),
        };
        let (p, created) = self
            .pk()
            .create_if_missing(
                &cx,
                &owner,
                eco,
                name,
                visibility,
                &repo_id,
                &opts.description,
            )
            .await?;
        let cx = if created {
            cx
        } else {
            self.pk().authorize(Some(caller), &p, Need::Write).await?
        };
        Ok((owner, p, cx, created))
    }

    /// The repo named `org/name`, when it exists.
    pub(crate) async fn resolve_repo(&self, full_name: &str) -> Option<Repo> {
        self.forge
            .orgs
            .resolve_repo(
                &self.lookup_cx(),
                &RepoRef {
                    repo: Some(repo_ref::Repo::FullName(full_name.to_string())),
                },
            )
            .await
            .ok()
    }

    /// `org/name` when `url` points at a repo of this forge (`https://host/org/name(.git)`,
    /// `git+https://...`, `git@host:org/name.git`).
    pub(crate) fn repo_from_url(&self, url: &str) -> Option<String> {
        let base = self.forge.packages.config().base_url.clone();
        let host = base
            .split("://")
            .nth(1)
            .unwrap_or(&base)
            .trim_end_matches('/')
            .split('/')
            .next()?
            .to_ascii_lowercase();
        let u = url.trim().trim_start_matches("git+");
        let rest = if let Some(r) = u.strip_prefix("git@") {
            r.replacen(':', "/", 1)
        } else {
            u.split("://").nth(1).unwrap_or(u).to_string()
        };
        let rest = rest.split('#').next().unwrap_or(&rest);
        let mut parts = rest.split('/');
        let h = parts.next()?.rsplit('@').next()?.to_ascii_lowercase();
        if h != host {
            return None;
        }
        let org = parts.next()?;
        let repo = parts.next()?.trim_end_matches(".git");
        (!org.is_empty() && !repo.is_empty()).then(|| format!("{org}/{repo}"))
    }

    /// The best-effort count of one download; failures never fail the download.
    pub(crate) async fn count_download(&self, p: &PackageRec, version_id: &str) {
        let _ = self
            .pk()
            .record_download(&self.lookup_cx(), p, version_id)
            .await;
    }

    /// Provenance claimed by the `x-bee-*` headers of a publish.
    pub(crate) fn claim(req: &Req) -> Option<ProvenanceClaim> {
        let run = header(req, "x-bee-run-id");
        if run.is_empty() {
            return None;
        }
        Some(ProvenanceClaim {
            repo_id: header(req, "x-bee-repo-id").to_string(),
            git_ref: header(req, "x-bee-ref").to_string(),
            changeset: crate::ids::unhex(header(req, "x-bee-changeset")).unwrap_or_default(),
            run_id: run.to_string(),
            workflow_path: header(req, "x-bee-workflow").to_string(),
            job_id: header(req, "x-bee-job").to_string(),
            signer_identity: header(req, "x-bee-signer").to_string(),
            issuer: header(req, "x-bee-issuer").to_string(),
            attestation_ids: header(req, "x-bee-attestations")
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        })
    }

    /// `org/name` of the repo the publisher names in `X-Bee-Repo`, to link a new package to.
    pub(crate) fn repo_hint(req: &Req) -> Option<String> {
        let v = header(req, "x-bee-repo");
        (!v.is_empty()).then(|| v.to_string())
    }
}

/// Splits `/<prefix>/<owner>/rest...` into the owner (percent-decoded) and the remaining raw
/// segments.
pub(crate) fn owner_and_rest<'a>(path: &'a str, prefix: &str) -> Option<(String, Vec<&'a str>)> {
    let mut segs = segments(path, prefix);
    if segs.is_empty() {
        return None;
    }
    let owner = super::http::pct_decode(segs.remove(0));
    Some((owner, segs))
}
