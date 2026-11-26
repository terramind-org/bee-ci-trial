//! `PagesService` over a [`ForgeServices`].
//!
//! | RPCs | repo permission |
//! |---|---|
//! | `GetPagesSite`, `ListPagesDeployments`, `GetPagesDeployment`, `GetPagesHealthCheck` | `read` |
//! | `RequestPagesBuild`, `CreatePagesDeployment`, `CancelPagesDeployment` | `commit` (write role; a job token with `commit` on the repo is the `pages: write` token) |
//! | `CreatePagesSite`, `UpdatePagesSite`, `DeletePagesSite`, `SetPagesCustomDomain`, `RequestPagesCertificate` | `manage_settings`, admin |
//! | `ListPagesPreviews` | `read` |
//! | the verified-domain RPCs, `UpdatePagesOrgSettings` | org owner or admin |
//! | `GetPagesOrgSettings` | org member |
//!
//! Denied state-changing calls are audited; writes to an archived repo are `ARCHIVED`.

use bee_policy::Action;
use bee_proto::v1::pages_service_server::PagesService;
use bee_proto::v1::{
    AddPagesVerifiedDomainRequest, CancelPagesDeploymentRequest, CreatePagesDeploymentRequest,
    CreatePagesSiteRequest, DeletePagesSiteRequest, DeletePagesSiteResponse,
    DeletePagesVerifiedDomainRequest, DeletePagesVerifiedDomainResponse, GetPagesDeploymentRequest,
    GetPagesHealthCheckRequest, GetPagesSiteRequest, ListPagesDeploymentsRequest,
    ListPagesDeploymentsResponse, ListPagesVerifiedDomainsRequest,
    ListPagesVerifiedDomainsResponse, PagesAccess, PagesBuildType, PagesCertificate,
    PagesDeployment, PagesDomainVerification, PagesHealthCheck, PagesSite, Repo, RepoRole,
    RequestPagesBuildRequest, RequestPagesCertificateRequest, SetPagesCustomDomainRequest,
    UpdatePagesSiteRequest, VerifyPagesDomainRequest,
};
use bee_proto::v1::{
    GetPagesOrgSettingsRequest, ListPagesPreviewsRequest, ListPagesPreviewsResponse,
    PagesOrgSettings, UpdatePagesOrgSettingsRequest,
};
use tonic::{Request, Response, Status};

use super::deploy::DeployOptions;
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::service::{ForgeServices, next, page, reply};

impl ForgeServices {
    async fn pages_repo(
        &self,
        caller: &Caller,
        repo: &Option<bee_proto::v1::RepoRef>,
        action: Action,
        min: Option<RepoRole>,
        audit: Option<&str>,
    ) -> Result<(Repo, Cx)> {
        let r = self.repo(caller, repo).await?;
        let auth = self
            .forge()
            .authz
            .repo(caller, &r, action, &[], None, min)
            .await;
        let cx = match audit {
            Some(a) => self.guarded(caller, &r.org_id, &r.id, a, auth).await?,
            None => auth?,
        };
        if audit.is_some() && r.archived {
            return Err(ForgeError::archived(&r.full_name));
        }
        Ok((r, cx))
    }

    async fn pages_org(&self, caller: &Caller, org: &str, audit: &str) -> Result<(String, Cx)> {
        let id = self.org_id(caller, org).await?;
        let auth = self.forge().authz.org(caller, &id, OrgPerm::Manage).await;
        let cx = self.guarded(caller, &id, "", audit, auth).await?;
        Ok((id, cx))
    }
}

#[tonic::async_trait]
impl PagesService for ForgeServices {
    async fn create_pages_site(
        &self,
        req: Request<CreatePagesSiteRequest>,
    ) -> Result<Response<PagesSite>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(
                    &caller,
                    &r.repo,
                    Action::ManageSettings,
                    Some(RepoRole::Admin),
                    Some("pages.site.create"),
                )
                .await?;
            let pages = &self.forge().pages;
            let s = pages
                .create_site(
                    &cx,
                    &repo,
                    PagesBuildType::try_from(r.build_type).unwrap_or_default(),
                    r.source,
                    PagesAccess::try_from(r.access).unwrap_or_default(),
                )
                .await?;
            Ok(pages.site_proto(&s))
        }
        .await;
        reply(&caller, out)
    }

    async fn get_pages_site(
        &self,
        req: Request<GetPagesSiteRequest>,
    ) -> Result<Response<PagesSite>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(&caller, &r.repo, Action::Read, None, None)
                .await?;
            let pages = &self.forge().pages;
            Ok(pages.site_proto(&pages.site(&cx, &repo).await?))
        }
        .await;
        reply(&caller, out)
    }

    async fn update_pages_site(
        &self,
        req: Request<UpdatePagesSiteRequest>,
    ) -> Result<Response<PagesSite>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(
                    &caller,
                    &r.repo,
                    Action::ManageSettings,
                    Some(RepoRole::Admin),
                    Some("pages.site.update"),
                )
                .await?;
            let pages = &self.forge().pages;
            let s = pages
                .update_site(
                    &cx,
                    &repo,
                    r.build_type
                        .map(|b| PagesBuildType::try_from(b).unwrap_or_default()),
                    r.source,
                    r.access
                        .map(|a| PagesAccess::try_from(a).unwrap_or_default()),
                    r.enforce_https,
                )
                .await?;
            let s = if r.previews_enabled.is_some() || r.preview_ttl_days.is_some() {
                pages
                    .set_preview_settings(
                        &cx,
                        &repo,
                        r.previews_enabled.unwrap_or(!s.previews_off),
                        r.preview_ttl_days.unwrap_or(0),
                    )
                    .await?
            } else {
                s
            };
            Ok(pages.site_proto(&s))
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_pages_site(
        &self,
        req: Request<DeletePagesSiteRequest>,
    ) -> Result<Response<DeletePagesSiteResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(
                    &caller,
                    &r.repo,
                    Action::ManageSettings,
                    Some(RepoRole::Admin),
                    Some("pages.site.delete"),
                )
                .await?;
            self.forge().pages.delete_site(&cx, &repo).await?;
            Ok(DeletePagesSiteResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_pages_deployments(
        &self,
        req: Request<ListPagesDeploymentsRequest>,
    ) -> Result<Response<ListPagesDeploymentsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(&caller, &r.repo, Action::Read, None, None)
                .await?;
            let pages = &self.forge().pages;
            let (token, limit) = page(&r.page);
            let (ds, nt) = pages.deployments(&cx, &repo, &token, limit).await?;
            Ok(ListPagesDeploymentsResponse {
                deployments: ds.iter().map(|d| pages.deployment_proto(d)).collect(),
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_pages_deployment(
        &self,
        req: Request<GetPagesDeploymentRequest>,
    ) -> Result<Response<PagesDeployment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out =
            async {
                let (repo, cx) = self
                    .pages_repo(&caller, &r.repo, Action::Read, None, None)
                    .await?;
                let pages = &self.forge().pages;
                Ok(pages
                    .deployment_proto(&pages.deployment(&cx, &repo, &r.pages_deployment_id).await?))
            }
            .await;
        reply(&caller, out)
    }

    async fn request_pages_build(
        &self,
        req: Request<RequestPagesBuildRequest>,
    ) -> Result<Response<PagesDeployment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(
                    &caller,
                    &r.repo,
                    Action::Commit,
                    Some(RepoRole::Write),
                    Some("pages.build.request"),
                )
                .await?;
            let pages = &self.forge().pages;
            Ok(pages.deployment_proto(
                &pages
                    .request_build(&cx, &repo, &DeployOptions::default())
                    .await?,
            ))
        }
        .await;
        reply(&caller, out)
    }

    async fn create_pages_deployment(
        &self,
        req: Request<CreatePagesDeploymentRequest>,
    ) -> Result<Response<PagesDeployment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(
                    &caller,
                    &r.repo,
                    Action::Commit,
                    Some(RepoRole::Write),
                    Some("pages.deployment.create"),
                )
                .await?;
            let pages = &self.forge().pages;
            let opts = DeployOptions {
                environment: r.environment,
                preview: r.preview,
                force: false,
            };
            let d = pages
                .create_deployment(
                    &cx,
                    &repo,
                    &r.artifact_id,
                    r.changeset.filter(|c| !c.raw.is_empty()),
                    &opts,
                )
                .await?;
            Ok(pages.deployment_proto(&d))
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_pages_deployment(
        &self,
        req: Request<CancelPagesDeploymentRequest>,
    ) -> Result<Response<PagesDeployment>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(
                    &caller,
                    &r.repo,
                    Action::Commit,
                    Some(RepoRole::Write),
                    Some("pages.deployment.cancel"),
                )
                .await?;
            let pages = &self.forge().pages;
            Ok(pages.deployment_proto(
                &pages
                    .cancel_deployment(&cx, &repo, &r.pages_deployment_id)
                    .await?,
            ))
        }
        .await;
        reply(&caller, out)
    }

    async fn set_pages_custom_domain(
        &self,
        req: Request<SetPagesCustomDomainRequest>,
    ) -> Result<Response<PagesSite>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(
                    &caller,
                    &r.repo,
                    Action::ManageSettings,
                    Some(RepoRole::Admin),
                    Some("pages.domain.set"),
                )
                .await?;
            let pages = &self.forge().pages;
            Ok(pages.site_proto(&pages.set_custom_domain(&cx, &repo, &r.domain).await?))
        }
        .await;
        reply(&caller, out)
    }

    async fn request_pages_certificate(
        &self,
        req: Request<RequestPagesCertificateRequest>,
    ) -> Result<Response<PagesCertificate>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(
                    &caller,
                    &r.repo,
                    Action::ManageSettings,
                    Some(RepoRole::Admin),
                    Some("pages.certificate.request"),
                )
                .await?;
            self.forge().pages.request_certificate(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_pages_health_check(
        &self,
        req: Request<GetPagesHealthCheckRequest>,
    ) -> Result<Response<PagesHealthCheck>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(&caller, &r.repo, Action::Read, None, None)
                .await?;
            self.forge().pages.health_check(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn add_pages_verified_domain(
        &self,
        req: Request<AddPagesVerifiedDomainRequest>,
    ) -> Result<Response<PagesDomainVerification>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out =
            async {
                let (org_id, cx) = self
                    .pages_org(&caller, &r.org, "pages.domain.verification.add")
                    .await?;
                let pages = &self.forge().pages;
                Ok(pages
                    .verification_proto(&pages.add_verified_domain(&cx, &org_id, &r.domain).await?))
            }
            .await;
        reply(&caller, out)
    }

    async fn list_pages_verified_domains(
        &self,
        req: Request<ListPagesVerifiedDomainsRequest>,
    ) -> Result<Response<ListPagesVerifiedDomainsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let cx = self
                .forge()
                .authz
                .org(&caller, &org_id, OrgPerm::Manage)
                .await?;
            let pages = &self.forge().pages;
            let (token, limit) = page(&r.page);
            let (vs, nt) = pages.verified_domains(&cx, &org_id, &token, limit).await?;
            Ok(ListPagesVerifiedDomainsResponse {
                domains: vs.iter().map(|v| pages.verification_proto(v)).collect(),
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn verify_pages_domain(
        &self,
        req: Request<VerifyPagesDomainRequest>,
    ) -> Result<Response<PagesDomainVerification>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .pages_org(&caller, &r.org, "pages.domain.verify")
                .await?;
            let pages = &self.forge().pages;
            Ok(pages.verification_proto(
                &pages
                    .verify_domain(&cx, &org_id, &r.verification_id)
                    .await?,
            ))
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_pages_verified_domain(
        &self,
        req: Request<DeletePagesVerifiedDomainRequest>,
    ) -> Result<Response<DeletePagesVerifiedDomainResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .pages_org(&caller, &r.org, "pages.domain.verification.delete")
                .await?;
            self.forge()
                .pages
                .delete_verified_domain(&cx, &org_id, &r.verification_id)
                .await?;
            Ok(DeletePagesVerifiedDomainResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_pages_previews(
        &self,
        req: Request<ListPagesPreviewsRequest>,
    ) -> Result<Response<ListPagesPreviewsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self
                .pages_repo(&caller, &r.repo, Action::Read, None, None)
                .await?;
            let pages = &self.forge().pages;
            Ok(ListPagesPreviewsResponse {
                previews: pages
                    .previews(&cx, &repo)
                    .await?
                    .iter()
                    .map(|d| pages.deployment_proto(d))
                    .collect(),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_pages_org_settings(
        &self,
        req: Request<GetPagesOrgSettingsRequest>,
    ) -> Result<Response<PagesOrgSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org_id = self.org_id(&caller, &r.org).await?;
            let cx = self
                .forge()
                .authz
                .org(&caller, &org_id, OrgPerm::ViewMembers)
                .await?;
            let s = self.forge().pages.org_settings(&cx, &org_id).await?;
            Ok(PagesOrgSettings {
                org: org_id,
                forbid_public: s.forbid_public,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_pages_org_settings(
        &self,
        req: Request<UpdatePagesOrgSettingsRequest>,
    ) -> Result<Response<PagesOrgSettings>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .pages_org(&caller, &r.org, "pages.settings.update")
                .await?;
            let pages = &self.forge().pages;
            if let Some(f) = r.forbid_public {
                pages.set_forbid_public(&cx, &org_id, f).await?;
            }
            let s = pages.org_settings(&cx, &org_id).await?;
            Ok(PagesOrgSettings {
                org: org_id,
                forbid_public: s.forbid_public,
            })
        }
        .await;
        reply(&caller, out)
    }
}
