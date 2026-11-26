//! `LeaseService`.

use bee_policy::Action;
use bee_proto::v1::lease_service_server::LeaseService;
use bee_proto::v1::{
    AcquireLeaseRequest, AcquireLeaseResponse, GetLeaseRequest, Lease, LeaseMode,
    ListLeasesRequest, ListLeasesResponse, ReleaseLeaseRequest, ReleaseLeaseResponse,
    RenewLeaseRequest, RepoRole,
};
use tonic::{Request, Response, Status};

use super::{ForgeServices, reply, utf8_paths};
use crate::error::ForgeError;
use crate::leases::AcquireRequest;

#[tonic::async_trait]
impl LeaseService for ForgeServices {
    async fn acquire_lease(
        &self,
        req: Request<AcquireLeaseRequest>,
    ) -> Result<Response<AcquireLeaseResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let ref_name = if r.r#ref.is_empty() {
                repo.default_ref.clone()
            } else {
                r.r#ref.clone()
            };
            let paths = utf8_paths(&r.paths)?;
            let auth = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Lease, &paths, Some(&ref_name), None)
                .await;
            let cx = self
                .guarded(&caller, &repo.org_id, &repo.id, "lease.acquire", auth)
                .await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            let (lease, overlapping) = self
                .forge
                .leases
                .acquire(
                    &cx,
                    AcquireRequest {
                        repo_id: repo.id.clone(),
                        org_id: repo.org_id.clone(),
                        ref_name,
                        paths: r.paths,
                        mode: LeaseMode::try_from(r.mode).unwrap_or(LeaseMode::Advisory),
                        ttl_seconds: r.ttl_seconds,
                        change_id: r.change_id,
                        note: r.note,
                    },
                )
                .await?;
            Ok(AcquireLeaseResponse {
                lease: Some(lease),
                overlapping,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn renew_lease(
        &self,
        req: Request<RenewLeaseRequest>,
    ) -> Result<Response<Lease>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let auth = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Lease, &[], None, None)
                .await;
            let cx = self
                .guarded(&caller, &repo.org_id, &repo.id, "lease.renew", auth)
                .await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            self.forge
                .leases
                .renew(&cx, &repo.org_id, &repo.id, &r.id, r.ttl_seconds)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn release_lease(
        &self,
        req: Request<ReleaseLeaseRequest>,
    ) -> Result<Response<ReleaseLeaseResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let lease = {
                let auth = self
                    .forge
                    .authz
                    .repo(&caller, &repo, Action::Read, &[], None, None)
                    .await;
                let cx = self
                    .guarded(&caller, &repo.org_id, &repo.id, "lease.release", auth)
                    .await?;
                self.forge.leases.get(&cx, &repo.id, &r.id).await?
            };
            let own = lease
                .holder
                .as_ref()
                .is_some_and(|h| h.id == caller.cx.actor_id);
            // The holder releases with `lease`; anyone else breaks a lease as a maintainer.
            let auth = if own {
                self.forge
                    .authz
                    .repo(&caller, &repo, Action::Lease, &[], None, None)
                    .await
            } else {
                self.forge
                    .authz
                    .repo(
                        &caller,
                        &repo,
                        Action::ManageSettings,
                        &[],
                        None,
                        Some(RepoRole::Maintain),
                    )
                    .await
            };
            let cx = self
                .guarded(&caller, &repo.org_id, &repo.id, "lease.release", auth)
                .await?;
            self.forge
                .leases
                .release(&cx, &repo.org_id, &repo.id, &r.id, !own)
                .await?;
            Ok(ReleaseLeaseResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_leases(
        &self,
        req: Request<ListLeasesRequest>,
    ) -> Result<Response<ListLeasesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            Ok(ListLeasesResponse {
                leases: self.forge.leases.list(&cx, &repo.id, &r.paths).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_lease(&self, req: Request<GetLeaseRequest>) -> Result<Response<Lease>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            self.forge.leases.get(&cx, &repo.id, &r.id).await
        }
        .await;
        reply(&caller, out)
    }
}
