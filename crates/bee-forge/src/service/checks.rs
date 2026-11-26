//! `CheckService`. Reporting checks needs the `commit` action (the write role): the policy
//! action set has no dedicated check-reporting action (an API gap in
//! `docs/spec/forge-core.md`).

use bee_policy::Action;
use bee_proto::v1::check_service_server::CheckService;
use bee_proto::v1::{
    Bisect, BisectSession, CancelBisectRequest, CheckRun, CheckStatus, CheckSuite, CombinedStatus,
    CommitStatus, CommitStatusState, CreateCheckSuiteRequest, CreateCommitStatusRequest,
    GetBisectNextCandidateRequest, GetBisectNextCandidateResponse, GetBisectRequest,
    GetBisectSessionRequest, GetCheckRunRequest, GetCombinedStatusRequest, ListBisectsRequest,
    ListBisectsResponse, ListCheckAnnotationsRequest, ListCheckAnnotationsResponse,
    ListCheckSuitesRequest, ListCheckSuitesResponse, ListChecksRequest, ListChecksResponse,
    ListCommitStatusesRequest, ListCommitStatusesResponse, MarkBisectRequest, Repo,
    ReportCheckRequest, RerequestCheckRunRequest, RerequestCheckSuiteRequest, StartBisectRequest,
    UpdateCheckRunRequest,
};
use tonic::{Request, Response, Status};

use super::{ForgeServices, next, page, reply, unserved};
use crate::auth::Caller;
use crate::checks::{Report, RunFilter, RunPatch};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{hex, ts_ms};

impl ForgeServices {
    async fn check_write(&self, caller: &Caller, repo: &Repo, action: &str) -> Result<Cx> {
        let auth = self
            .forge
            .authz
            .repo(caller, repo, Action::Commit, &[], None, None)
            .await;
        let cx = self
            .guarded(caller, &repo.org_id, &repo.id, action, auth)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        Ok(cx)
    }
}

#[tonic::async_trait]
impl CheckService for ForgeServices {
    async fn report_check(
        &self,
        req: Request<ReportCheckRequest>,
    ) -> Result<Response<CheckRun>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.check_write(&caller, &repo, "check.report").await?;
            self.forge
                .checks
                .report(
                    &cx,
                    &repo.org_id,
                    &repo.id,
                    Report {
                        changeset: r.changeset,
                        name: r.name,
                        status: CheckStatus::try_from(r.status).unwrap_or(CheckStatus::Unspecified),
                        url: r.url,
                        summary: r.summary,
                        post_land: r.post_land,
                        external_id: r.external_id,
                        output: r.output,
                        suite_id: r.suite_id,
                        actions: r.actions,
                        started_at_ms: r.started_at.as_ref().map(ts_ms).unwrap_or(0),
                        finished_at_ms: r.finished_at.as_ref().map(ts_ms).unwrap_or(0),
                        workflow_job_id: String::new(),
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_checks(
        &self,
        req: Request<ListChecksRequest>,
    ) -> Result<Response<ListChecksResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let filter = RunFilter {
                changeset_hex: r
                    .changeset
                    .as_ref()
                    .filter(|c| !c.raw.is_empty())
                    .map(|c| hex(&c.raw)),
                name: r.name,
                suite_id: r.suite_id,
                latest_only: r.latest_only,
            };
            Ok(ListChecksResponse {
                checks: self.forge.checks.list(&cx, &repo.id, &filter).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_check_run(
        &self,
        req: Request<GetCheckRunRequest>,
    ) -> Result<Response<CheckRun>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            self.forge.checks.get(&cx, &repo.id, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_check_run(
        &self,
        req: Request<UpdateCheckRunRequest>,
    ) -> Result<Response<CheckRun>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.check_write(&caller, &repo, "check.update").await?;
            let patch = RunPatch {
                status: r
                    .status
                    .map(|s| CheckStatus::try_from(s).unwrap_or(CheckStatus::Unspecified)),
                url: r.url,
                summary: r.summary,
                output: r.output,
                actions: r.actions,
                finished_at_ms: r.finished_at.as_ref().map(ts_ms).unwrap_or(0),
            };
            self.forge
                .checks
                .update(&cx, &repo.org_id, &repo.id, &r.id, patch)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_check_annotations(
        &self,
        req: Request<ListCheckAnnotationsRequest>,
    ) -> Result<Response<ListCheckAnnotationsResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let (after, size) = page(&r.page);
            let (annotations, token) = self
                .forge
                .checks
                .annotations(&cx, &repo.id, &r.check_run_id, &after, size)
                .await?;
            Ok(ListCheckAnnotationsResponse {
                annotations,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn rerequest_check_run(
        &self,
        req: Request<RerequestCheckRunRequest>,
    ) -> Result<Response<CheckRun>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.check_write(&caller, &repo, "check.rerequest").await?;
            self.forge
                .checks
                .rerequest_run(&cx, &repo.org_id, &repo.id, &r.id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_check_suite(
        &self,
        req: Request<CreateCheckSuiteRequest>,
    ) -> Result<Response<CheckSuite>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .check_write(&caller, &repo, "check_suite.create")
                .await?;
            self.forge
                .checks
                .create_suite(&cx, &repo.org_id, &repo.id, r.changeset, &r.head_ref)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_check_suites(
        &self,
        req: Request<ListCheckSuitesRequest>,
    ) -> Result<Response<ListCheckSuitesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let cs = r
                .changeset
                .as_ref()
                .filter(|c| !c.raw.is_empty())
                .map(|c| hex(&c.raw));
            Ok(ListCheckSuitesResponse {
                suites: self
                    .forge
                    .checks
                    .list_suites(&cx, &repo.id, cs.as_deref(), &r.app_id)
                    .await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn rerequest_check_suite(
        &self,
        req: Request<RerequestCheckSuiteRequest>,
    ) -> Result<Response<CheckSuite>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .check_write(&caller, &repo, "check_suite.rerequest")
                .await?;
            self.forge
                .checks
                .rerequest_suite(&cx, &repo.org_id, &repo.id, &r.id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_commit_status(
        &self,
        req: Request<CreateCommitStatusRequest>,
    ) -> Result<Response<CommitStatus>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self.check_write(&caller, &repo, "status.create").await?;
            let state =
                CommitStatusState::try_from(r.state).unwrap_or(CommitStatusState::Unspecified);
            self.forge
                .checks
                .create_status(
                    &cx,
                    &repo.org_id,
                    &repo.id,
                    r.changeset,
                    &r.context,
                    state,
                    &r.target_url,
                    &r.description,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_commit_statuses(
        &self,
        req: Request<ListCommitStatusesRequest>,
    ) -> Result<Response<ListCommitStatusesResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let cs = r
                .changeset
                .as_ref()
                .filter(|c| !c.raw.is_empty())
                .map(|c| hex(&c.raw))
                .ok_or_else(|| ForgeError::missing("changeset"))?;
            let (after, size) = page(&r.page);
            let (statuses, token) = self
                .forge
                .checks
                .list_statuses(&cx, &repo.id, &cs, &after, size)
                .await?;
            Ok(ListCommitStatusesResponse {
                statuses,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_combined_status(
        &self,
        req: Request<GetCombinedStatusRequest>,
    ) -> Result<Response<CombinedStatus>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.repo(&caller, &r.repo).await?;
            let cx = self
                .forge
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let expr =
                r.at.map(|a| a.expr)
                    .filter(|e| !e.is_empty())
                    .ok_or_else(|| ForgeError::missing("at"))?;
            let cs = self.forge.hooks.resolver.resolve(&repo, &expr).await?;
            self.forge.checks.combined_status(&cx, &repo.id, cs).await
        }
        .await;
        reply(&caller, out)
    }

    async fn start_bisect(
        &self,
        req: Request<StartBisectRequest>,
    ) -> Result<Response<Bisect>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "CheckService.StartBisect",
                "ci (post-land bisect)",
            )),
        )
    }

    async fn get_bisect(&self, req: Request<GetBisectRequest>) -> Result<Response<Bisect>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved("CheckService.GetBisect", "ci (post-land bisect)")),
        )
    }

    async fn list_bisects(
        &self,
        req: Request<ListBisectsRequest>,
    ) -> Result<Response<ListBisectsResponse>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "CheckService.ListBisects",
                "ci (post-land bisect)",
            )),
        )
    }

    async fn get_bisect_session(
        &self,
        req: Request<GetBisectSessionRequest>,
    ) -> Result<Response<BisectSession>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "CheckService.GetBisectSession",
                "ci (post-land bisect)",
            )),
        )
    }

    async fn get_bisect_next_candidate(
        &self,
        req: Request<GetBisectNextCandidateRequest>,
    ) -> Result<Response<GetBisectNextCandidateResponse>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "CheckService.GetBisectNextCandidate",
                "ci (post-land bisect)",
            )),
        )
    }

    async fn mark_bisect(
        &self,
        req: Request<MarkBisectRequest>,
    ) -> Result<Response<Bisect>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved("CheckService.MarkBisect", "ci (post-land bisect)")),
        )
    }

    async fn cancel_bisect(
        &self,
        req: Request<CancelBisectRequest>,
    ) -> Result<Response<Bisect>, Status> {
        let caller = self.caller(&req)?;
        reply(
            &caller,
            Err(unserved(
                "CheckService.CancelBisect",
                "ci (post-land bisect)",
            )),
        )
    }
}
