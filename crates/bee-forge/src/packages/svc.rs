//! `PackageService`: package, version and access management over a [`ForgeServices`].
//!
//! Permission levels (see [`super::access`]): reads (`ListPackages`, `GetPackage`,
//! `ListPackageVersions`, `GetPackageVersion`, provenance, usage) need `read`; publishing a
//! version natively and moving tags need `write`; deleting or restoring a package or version,
//! visibility, repo links and access grants need `admin`. Creating a package (the first
//! `BeginPackageUpload`) needs ownership, org admin, or write on the repo it links to.
//! State-changing calls that fail authorization are audited as `package.*` denials.

use bee_policy::Action;
use bee_proto::v1::package_service_server::PackageService;
use bee_proto::v1::{
    BeginPackageUploadRequest, BeginPackageUploadResponse, DeletePackageRequest,
    DeletePackageResponse, DeletePackageVersionRequest, DeletePackageVersionResponse,
    FinalizePackageVersionRequest, GetPackageRequest, GetPackageUsageRequest,
    GetPackageVersionProvenanceRequest, GetPackageVersionRequest, GrantPackageAccessRequest,
    KeyValue, LinkPackageToRepoRequest, ListPackageAccessGrantsRequest,
    ListPackageAccessGrantsResponse, ListPackageVersionsRequest, ListPackageVersionsResponse,
    ListPackagesRequest, ListPackagesResponse, Package, PackageAccessGrant, PackageGranteeKind,
    PackageUploadTarget, PackageUsage, PackageVersion, PackageVersionProvenance, PackageVisibility,
    RepoRole, RestorePackageRequest, RestorePackageVersionRequest, RevokePackageAccessRequest,
    RevokePackageAccessResponse, SetPackageVersionTagsRequest, SetPackageVisibilityRequest,
    UnlinkPackageFromRepoRequest, UpdatePackageRequest,
};
use bee_proto::v1::{
    GetPackageOwnerSettingsRequest, GetPackageRetentionPolicyRequest, PackageOwnerKind,
    PackageOwnerSettings, PackageRetentionPolicy, Role, SetPackageRetentionPolicyRequest,
    UpdatePackageOwnerSettingsRequest,
};
use tonic::{Request, Response, Status};

use super::access::Need;
use super::lifecycle::UploadFileSpec;
use super::manage::{ListFilter, Owner};
use super::model::{Eco, PackageRec, RetentionRec};
use crate::auth::Caller;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::service::{ForgeServices, next, page, reply};

impl ForgeServices {
    async fn pkg_target(
        &self,
        caller: &Caller,
        owner: &str,
        eco: i32,
        name: &str,
        need: Need,
        audit: Option<&str>,
    ) -> Result<(Owner, PackageRec, Cx)> {
        let eco = Eco::from_number(eco)?;
        let pk = &self.forge().packages;
        let o = pk.owner(&caller.cx, owner).await?;
        let p = pk.get(&caller.cx, &o, eco, name).await?;
        let auth = pk.authorize(Some(caller), &p, need).await;
        let cx = match audit {
            Some(a) => {
                self.guarded(caller, o.org_id(), &p.repo_id, a, auth)
                    .await?
            }
            None => auth?,
        };
        Ok((o, p, cx))
    }

    async fn readable_packages(
        &self,
        caller: &Caller,
        f: &ListFilter,
        token: &str,
        limit: usize,
    ) -> Result<(Vec<PackageRec>, String)> {
        let pk = &self.forge().packages;
        let mut out = Vec::new();
        let mut cursor = token.to_string();
        loop {
            let (rows, nt) = pk.list(&caller.cx, f, &cursor, limit).await?;
            for p in rows {
                let need = if p.deleted_ms != 0 {
                    Need::Admin
                } else {
                    Need::Read
                };
                if pk.authorize(Some(caller), &p, need).await.is_ok() {
                    out.push(p);
                }
            }
            if out.len() >= limit || nt.is_empty() {
                return Ok((out, nt));
            }
            cursor = nt;
        }
    }
}

fn vis_filter(v: i32) -> Option<PackageVisibility> {
    PackageVisibility::try_from(v)
        .ok()
        .filter(|v| *v != PackageVisibility::Unspecified)
}

fn kv(k: &str, v: &str) -> KeyValue {
    KeyValue {
        key: k.into(),
        value: v.into(),
    }
}

#[tonic::async_trait]
impl PackageService for ForgeServices {
    async fn list_packages(
        &self,
        req: Request<ListPackagesRequest>,
    ) -> Result<Response<ListPackagesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if !caller.token.allows_action(Action::Read) {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::PolicyDenied,
                    "token does not allow `read`",
                ));
            }
            let pk = &self.forge().packages;
            let mut f = ListFilter {
                eco: Eco::filter(r.ecosystem)?,
                visibility: vis_filter(r.visibility),
                include_deleted: r.include_deleted,
                ..Default::default()
            };
            if !r.owner.is_empty() {
                f.owner_id = pk.owner(&caller.cx, &r.owner).await?.id;
            }
            if r.repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                // A repo the caller cannot read answers NOT_FOUND, like every other repo read,
                // rather than an empty list that would tell a private repo from a missing one.
                let repo = self.repo(&caller, &r.repo).await?;
                self.forge()
                    .authz
                    .repo(&caller, &repo, Action::Read, &[], None, None)
                    .await?;
                f.repo_id = repo.id;
            }
            let (token, limit) = page(&r.page);
            let (rows, nt) = self.readable_packages(&caller, &f, &token, limit).await?;
            let mut packages = Vec::new();
            for p in &rows {
                packages.push(pk.package_proto(&caller.cx, p).await?);
            }
            Ok(ListPackagesResponse {
                packages,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_package(
        &self,
        req: Request<GetPackageRequest>,
    ) -> Result<Response<Package>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, p, cx) = self
                .pkg_target(&caller, &r.owner, r.ecosystem, &r.name, Need::Read, None)
                .await?;
            self.forge().packages.package_proto(&cx, &p).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_package(
        &self,
        req: Request<UpdatePackageRequest>,
    ) -> Result<Response<Package>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.update"),
                )
                .await?;
            let pk = &self.forge().packages;
            let p = pk
                .update(&cx, &o, &p, r.description, r.inherit_access_from_repo)
                .await?;
            pk.package_proto(&cx, &p).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_package(
        &self,
        req: Request<DeletePackageRequest>,
    ) -> Result<Response<DeletePackageResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.delete"),
                )
                .await?;
            self.forge().packages.delete(&cx, &o, &p).await?;
            Ok(DeletePackageResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn restore_package(
        &self,
        req: Request<RestorePackageRequest>,
    ) -> Result<Response<Package>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let eco = Eco::from_number(r.ecosystem)?;
            let pk = &self.forge().packages;
            let o = pk.owner(&caller.cx, &r.owner).await?;
            let Some(deleted) = pk.find_deleted(&caller.cx, &o, eco, &r.name).await? else {
                return Err(ForgeError::not_found(format!("deleted package {}", r.name)));
            };
            let auth = pk.authorize(Some(&caller), &deleted, Need::Admin).await;
            let cx = self
                .guarded(
                    &caller,
                    o.org_id(),
                    &deleted.repo_id,
                    "package.restore",
                    auth,
                )
                .await?;
            let p = pk.restore(&cx, &o, eco, &r.name).await?;
            pk.package_proto(&cx, &p).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_package_visibility(
        &self,
        req: Request<SetPackageVisibilityRequest>,
    ) -> Result<Response<Package>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.visibility.set"),
                )
                .await?;
            let v =
                PackageVisibility::try_from(r.visibility).unwrap_or(PackageVisibility::Unspecified);
            let pk = &self.forge().packages;
            let p = pk.set_visibility(&cx, &o, &p, v).await?;
            pk.package_proto(&cx, &p).await
        }
        .await;
        reply(&caller, out)
    }

    async fn link_package_to_repo(
        &self,
        req: Request<LinkPackageToRepoRequest>,
    ) -> Result<Response<Package>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.link"),
                )
                .await?;
            let repo = self.repo(&caller, &r.repo).await?;
            let auth = self
                .forge()
                .authz
                .repo(
                    &caller,
                    &repo,
                    Action::Commit,
                    &[],
                    None,
                    Some(RepoRole::Write),
                )
                .await;
            self.guarded(&caller, o.org_id(), &repo.id, "package.link", auth)
                .await?;
            if o.kind == bee_proto::v1::PackageOwnerKind::Org && repo.org_id != o.id {
                return Err(ForgeError::permission_denied(
                    "an org package can only be linked to a repo of the same org",
                ));
            }
            let pk = &self.forge().packages;
            let p = pk.link_repo(&cx, &o, &p, &repo.id).await?;
            pk.package_proto(&cx, &p).await
        }
        .await;
        reply(&caller, out)
    }

    async fn unlink_package_from_repo(
        &self,
        req: Request<UnlinkPackageFromRepoRequest>,
    ) -> Result<Response<Package>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.unlink"),
                )
                .await?;
            let pk = &self.forge().packages;
            let p = pk.unlink_repo(&cx, &o, &p).await?;
            pk.package_proto(&cx, &p).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_package_access_grants(
        &self,
        req: Request<ListPackageAccessGrantsRequest>,
    ) -> Result<Response<ListPackageAccessGrantsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, p, cx) = self
                .pkg_target(&caller, &r.owner, r.ecosystem, &r.name, Need::Admin, None)
                .await?;
            let (token, limit) = page(&r.page);
            let (grants, nt) = self.forge().packages.grants(&cx, &p, &token, limit).await?;
            Ok(ListPackageAccessGrantsResponse {
                grants,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn grant_package_access(
        &self,
        req: Request<GrantPackageAccessRequest>,
    ) -> Result<Response<PackageAccessGrant>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.access.grant"),
                )
                .await?;
            let kind =
                PackageGranteeKind::try_from(r.kind).unwrap_or(PackageGranteeKind::Unspecified);
            let role = bee_proto::v1::PackageRole::try_from(r.role).unwrap_or_default();
            let (ident, repo_id) = match kind {
                PackageGranteeKind::Repo => {
                    let repo = self.repo(&caller, &r.grantee_repo).await?;
                    if o.kind == bee_proto::v1::PackageOwnerKind::Org && repo.org_id != o.id {
                        return Err(ForgeError::invalid(
                            "bad_grantee",
                            "a repo grant names a repo of the owning org",
                        ));
                    }
                    (repo.id.clone(), repo.id)
                }
                PackageGranteeKind::Team => (r.team_id.clone(), String::new()),
                PackageGranteeKind::Principal => (r.principal_id.clone(), String::new()),
                PackageGranteeKind::Unspecified => return Err(ForgeError::missing("kind")),
            };
            self.forge()
                .packages
                .grant(&cx, &o, &p, kind, &ident, &repo_id, role)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn revoke_package_access(
        &self,
        req: Request<RevokePackageAccessRequest>,
    ) -> Result<Response<RevokePackageAccessResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.access.revoke"),
                )
                .await?;
            let kind =
                PackageGranteeKind::try_from(r.kind).unwrap_or(PackageGranteeKind::Unspecified);
            let ident = match kind {
                PackageGranteeKind::Repo => self.repo(&caller, &r.grantee_repo).await?.id,
                PackageGranteeKind::Team => r.team_id.clone(),
                PackageGranteeKind::Principal => r.principal_id.clone(),
                PackageGranteeKind::Unspecified => return Err(ForgeError::missing("kind")),
            };
            self.forge()
                .packages
                .revoke(&cx, &o, &p, kind, &ident)
                .await?;
            Ok(RevokePackageAccessResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_package_versions(
        &self,
        req: Request<ListPackageVersionsRequest>,
    ) -> Result<Response<ListPackageVersionsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, p, cx) = self
                .pkg_target(&caller, &r.owner, r.ecosystem, &r.name, Need::Read, None)
                .await?;
            let pk = &self.forge().packages;
            let include_deleted =
                r.include_deleted && pk.authorize(Some(&caller), &p, Need::Admin).await.is_ok();
            let (token, limit) = page(&r.page);
            let (vs, nt) = pk
                .versions(&cx, &p, include_deleted, r.tagged_only, &token, limit)
                .await?;
            let mut versions = Vec::new();
            for v in &vs {
                versions.push(pk.version_proto(&cx, &p, v, false).await?);
            }
            Ok(ListPackageVersionsResponse {
                versions,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_package_version(
        &self,
        req: Request<GetPackageVersionRequest>,
    ) -> Result<Response<PackageVersion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, p, cx) = self
                .pkg_target(&caller, &r.owner, r.ecosystem, &r.name, Need::Read, None)
                .await?;
            let pk = &self.forge().packages;
            let v = pk.version(&cx, &p, &r.version).await?;
            pk.version_proto(&cx, &p, &v, r.include_download_urls).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_package_version(
        &self,
        req: Request<DeletePackageVersionRequest>,
    ) -> Result<Response<DeletePackageVersionResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.version.delete"),
                )
                .await?;
            self.forge()
                .packages
                .delete_version(&cx, &o, &p, &r.version)
                .await?;
            Ok(DeletePackageVersionResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn restore_package_version(
        &self,
        req: Request<RestorePackageVersionRequest>,
    ) -> Result<Response<PackageVersion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.version.restore"),
                )
                .await?;
            let pk = &self.forge().packages;
            let v = pk.restore_version(&cx, &o, &p, &r.version_id).await?;
            pk.version_proto(&cx, &p, &v, false).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_package_version_tags(
        &self,
        req: Request<SetPackageVersionTagsRequest>,
    ) -> Result<Response<PackageVersion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Write,
                    Some("package.tags.set"),
                )
                .await?;
            let pk = &self.forge().packages;
            let v = pk.set_tags(&cx, &o, &p, &r.version, &r.tags).await?;
            pk.version_proto(&cx, &p, &v, false).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_package_version_provenance(
        &self,
        req: Request<GetPackageVersionProvenanceRequest>,
    ) -> Result<Response<PackageVersionProvenance>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, p, cx) = self
                .pkg_target(&caller, &r.owner, r.ecosystem, &r.name, Need::Read, None)
                .await?;
            self.forge().packages.provenance(&cx, &p, &r.version).await
        }
        .await;
        reply(&caller, out)
    }

    async fn begin_package_upload(
        &self,
        req: Request<BeginPackageUploadRequest>,
    ) -> Result<Response<BeginPackageUploadResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let eco = Eco::from_number(r.ecosystem)?;
            let pk = &self.forge().packages;
            let o = pk.owner(&caller.cx, &r.owner).await?;
            let (p, cx) = match pk.find(&caller.cx, &o, eco, &r.name).await? {
                Some(p) => {
                    let auth = pk.authorize(Some(&caller), &p, Need::Write).await;
                    let cx = self
                        .guarded(
                            &caller,
                            o.org_id(),
                            &p.repo_id,
                            "package.upload.begin",
                            auth,
                        )
                        .await?;
                    (p, cx)
                }
                None => {
                    let repo = if r.link_repo.as_ref().is_some_and(|x| x.repo.is_some()) {
                        Some(self.repo(&caller, &r.link_repo).await?)
                    } else {
                        None
                    };
                    let auth = pk.authorize_create(&caller, &o, repo.as_ref()).await;
                    let cx = self
                        .guarded(
                            &caller,
                            o.org_id(),
                            repo.as_ref().map(|x| x.id.as_str()).unwrap_or(""),
                            "package.create",
                            auth,
                        )
                        .await?;
                    let vis = vis_filter(r.visibility).unwrap_or(match &repo {
                        Some(rp) => match bee_proto::v1::Visibility::try_from(rp.visibility)
                            .unwrap_or_default()
                        {
                            bee_proto::v1::Visibility::Public => PackageVisibility::Public,
                            bee_proto::v1::Visibility::Internal => PackageVisibility::Internal,
                            _ => PackageVisibility::Private,
                        },
                        None => PackageVisibility::Private,
                    });
                    let repo_id = repo.as_ref().map(|x| x.id.clone()).unwrap_or_default();
                    let (p, _) = pk
                        .create_if_missing(&cx, &o, eco, &r.name, vis, &repo_id, "")
                        .await?;
                    (p, cx)
                }
            };
            let files: Vec<UploadFileSpec> = r
                .files
                .iter()
                .map(|f| UploadFileSpec {
                    name: f.name.clone(),
                    size: f.size_bytes,
                    digest: f.digest.clone(),
                    media_type: f.media_type.clone(),
                })
                .collect();
            let (u, targets) = pk
                .begin_upload(&cx, &o, &p, &r.version, &files, &r.metadata_json)
                .await?;
            Ok(BeginPackageUploadResponse {
                upload_id: u.id,
                targets: targets
                    .into_iter()
                    .map(|t| PackageUploadTarget {
                        name: t.name,
                        upload_url: t.url,
                        upload_headers: t.headers.iter().map(|(k, v)| kv(k, v)).collect(),
                    })
                    .collect(),
                expires_at: Some(ts(u.expires_ms)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn finalize_package_version(
        &self,
        req: Request<FinalizePackageVersionRequest>,
    ) -> Result<Response<PackageVersion>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let pk = &self.forge().packages;
            let o = pk.owner(&caller.cx, &r.owner).await?;
            let u = pk.native_upload(&caller.cx, &o, &r.upload_id).await?;
            let p = pk.get_by_id(&caller.cx, &o.id, &u.package_id).await?;
            let auth = pk.authorize(Some(&caller), &p, Need::Write).await;
            let cx = self
                .guarded(
                    &caller,
                    o.org_id(),
                    &p.repo_id,
                    "package.upload.finalize",
                    auth,
                )
                .await?;
            let v = pk.finalize_upload(&cx, &o, &p, &u, &r.tags, None).await?;
            pk.version_proto(&cx, &p, &v, false).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_package_usage(
        &self,
        req: Request<GetPackageUsageRequest>,
    ) -> Result<Response<PackageUsage>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, p, cx) = self
                .pkg_target(&caller, &r.owner, r.ecosystem, &r.name, Need::Read, None)
                .await?;
            self.forge()
                .packages
                .usage(&cx, &p, r.since.as_ref().map(ts_ms))
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_package_retention_policy(
        &self,
        req: Request<GetPackageRetentionPolicyRequest>,
    ) -> Result<Response<PackageRetentionPolicy>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let pk = &self.forge().packages;
            if r.name.is_empty() {
                let (o, cx) = self.pkg_owner_admin(&caller, &r.owner, None).await?;
                let p = pk.retention_of(&cx, &o.id, None).await?.unwrap_or_default();
                return Ok(retention_proto(&o, &r, &p, false));
            }
            let (o, p, cx) = self
                .pkg_target(&caller, &r.owner, r.ecosystem, &r.name, Need::Read, None)
                .await?;
            let own = pk.retention_of(&cx, &o.id, Some(&p)).await?;
            let inherited = own.is_none();
            let policy = match own {
                Some(x) => x,
                None => pk.retention_of(&cx, &o.id, None).await?.unwrap_or_default(),
            };
            Ok(retention_proto(&o, &r, &policy, inherited))
        }
        .await;
        reply(&caller, out)
    }

    async fn set_package_retention_policy(
        &self,
        req: Request<SetPackageRetentionPolicyRequest>,
    ) -> Result<Response<PackageRetentionPolicy>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let pk = &self.forge().packages;
            let policy = RetentionRec {
                keep_last: r.keep_last,
                untagged_days: r.untagged_days,
                older_than_days: r.older_than_days,
            };
            let get = GetPackageRetentionPolicyRequest {
                owner: r.owner.clone(),
                ecosystem: r.ecosystem,
                name: r.name.clone(),
            };
            if r.name.is_empty() {
                let (o, cx) = self
                    .pkg_owner_admin(&caller, &r.owner, Some("package.retention.set"))
                    .await?;
                pk.set_retention(&cx, &o, None, policy.clone()).await?;
                return Ok(retention_proto(&o, &get, &policy, false));
            }
            let (o, p, cx) = self
                .pkg_target(
                    &caller,
                    &r.owner,
                    r.ecosystem,
                    &r.name,
                    Need::Admin,
                    Some("package.retention.set"),
                )
                .await?;
            pk.set_retention(&cx, &o, Some(&p), policy.clone()).await?;
            Ok(retention_proto(&o, &get, &policy, false))
        }
        .await;
        reply(&caller, out)
    }

    async fn get_package_owner_settings(
        &self,
        req: Request<GetPackageOwnerSettingsRequest>,
    ) -> Result<Response<PackageOwnerSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, cx) = self.pkg_owner_admin(&caller, &r.owner, None).await?;
            let s = self.forge().packages.owner_settings(&cx, &o).await?;
            Ok(PackageOwnerSettings {
                owner: o.name,
                forbid_public: s.forbid_public,
                storage_quota_bytes: s.storage_quota_bytes,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_package_owner_settings(
        &self,
        req: Request<UpdatePackageOwnerSettingsRequest>,
    ) -> Result<Response<PackageOwnerSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (o, cx) = self
                .pkg_owner_admin(&caller, &r.owner, Some("package.settings.update"))
                .await?;
            let s = self
                .forge()
                .packages
                .set_owner_settings(&cx, &o, r.forbid_public, r.storage_quota_bytes)
                .await?;
            Ok(PackageOwnerSettings {
                owner: o.name,
                forbid_public: s.forbid_public,
                storage_quota_bytes: s.storage_quota_bytes,
            })
        }
        .await;
        reply(&caller, out)
    }
}

fn retention_proto(
    o: &Owner,
    r: &GetPackageRetentionPolicyRequest,
    p: &RetentionRec,
    inherited: bool,
) -> PackageRetentionPolicy {
    PackageRetentionPolicy {
        owner: o.name.clone(),
        ecosystem: if r.name.is_empty() { 0 } else { r.ecosystem },
        name: r.name.clone(),
        keep_last: p.keep_last,
        untagged_days: p.untagged_days,
        older_than_days: p.older_than_days,
        inherited,
    }
}

impl ForgeServices {
    /// Owner administration: the user owner, or an org owner/admin, with a token that allows
    /// `manage_settings` and is not restricted to repos. Denials of changes are audited.
    async fn pkg_owner_admin(
        &self,
        caller: &Caller,
        owner: &str,
        audit: Option<&str>,
    ) -> Result<(Owner, Cx)> {
        if owner.is_empty() {
            return Err(ForgeError::missing("owner"));
        }
        let o = self.forge().packages.owner(&caller.cx, owner).await?;
        let auth = async {
            if !caller.token.allows_action(Action::ManageSettings)
                || caller.token.scope.repos.is_some()
            {
                return Err(ForgeError::new(
                    bee_proto::v1::ErrorReason::PolicyDenied,
                    "the token must allow `manage_settings` and not be restricted to repos",
                ));
            }
            for id in &caller.cx.chain {
                let ok = match o.kind {
                    PackageOwnerKind::User => *id == o.id,
                    _ => matches!(
                        self.forge()
                            .orgs
                            .member(&caller.cx, &o.id, id)
                            .await?
                            .map(|m| Role::try_from(m.role).unwrap_or(Role::Member)),
                        Some(Role::Owner | Role::Admin)
                    ),
                };
                if !ok {
                    return Err(ForgeError::permission_denied(format!(
                        "{id} does not administer packages of {}",
                        o.name
                    )));
                }
            }
            let mut cx = caller.cx.clone();
            cx.decision = format!("token:ok owner-admin:{}", o.name);
            Ok(cx)
        }
        .await;
        let cx = match audit {
            Some(a) => self.guarded(caller, o.org_id(), "", a, auth).await?,
            None => auth?,
        };
        Ok((o, cx))
    }
}
