//! `CheckService` with the bisect RPCs served: [`BisectCheckService`] answers
//! `StartBisect`, `GetBisect`, `ListBisects`, `GetBisectSession`, `GetBisectNextCandidate`,
//! `MarkBisect` and `CancelBisect` from a [`Bisector`] and delegates every other
//! RPC to the core [`ForgeServices`] implementation. The server registers it in place of the
//! core `CheckService`.
//!
//! | RPCs | authorization |
//! |---|---|
//! | `GetBisect`, `ListBisects`, `GetBisectSession`, `GetBisectNextCandidate` | `read` |
//! | `StartBisect`, `MarkBisect`, `CancelBisect` | `commit` (write role) |

use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::check_service_server::{CheckService, CheckServiceServer};
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::plan::Outcome;
use super::{Bisector, StartBisect};
use crate::auth::Caller;
use crate::ci::service::paged;
use crate::cx::Cx;
use crate::error::ForgeError;
use crate::error::Result;
use crate::forge::Forge;
use crate::service::{ForgeServices, reply};

type Reply<T> = std::result::Result<Response<T>, Status>;

/// `CheckService` with post-land bisect.
#[derive(Clone)]
pub struct BisectCheckService {
    base: ForgeServices,
    bisector: Arc<Bisector>,
}

impl BisectCheckService {
    /// The service over `forge` and `bisector`.
    pub fn new(forge: Arc<Forge>, bisector: Arc<Bisector>) -> BisectCheckService {
        BisectCheckService {
            base: ForgeServices::new(forge),
            bisector,
        }
    }

    /// The tonic server.
    pub fn server(&self) -> CheckServiceServer<BisectCheckService> {
        CheckServiceServer::new(self.clone())
    }
}

#[tonic::async_trait]
impl CheckService for BisectCheckService {
    async fn report_check(&self, req: Request<ReportCheckRequest>) -> Reply<CheckRun> {
        CheckService::report_check(&self.base, req).await
    }

    async fn list_checks(&self, req: Request<ListChecksRequest>) -> Reply<ListChecksResponse> {
        CheckService::list_checks(&self.base, req).await
    }

    async fn get_check_run(&self, req: Request<GetCheckRunRequest>) -> Reply<CheckRun> {
        CheckService::get_check_run(&self.base, req).await
    }

    async fn update_check_run(&self, req: Request<UpdateCheckRunRequest>) -> Reply<CheckRun> {
        CheckService::update_check_run(&self.base, req).await
    }

    async fn list_check_annotations(
        &self,
        req: Request<ListCheckAnnotationsRequest>,
    ) -> Reply<ListCheckAnnotationsResponse> {
        CheckService::list_check_annotations(&self.base, req).await
    }

    async fn rerequest_check_run(&self, req: Request<RerequestCheckRunRequest>) -> Reply<CheckRun> {
        CheckService::rerequest_check_run(&self.base, req).await
    }

    async fn create_check_suite(&self, req: Request<CreateCheckSuiteRequest>) -> Reply<CheckSuite> {
        CheckService::create_check_suite(&self.base, req).await
    }

    async fn list_check_suites(
        &self,
        req: Request<ListCheckSuitesRequest>,
    ) -> Reply<ListCheckSuitesResponse> {
        CheckService::list_check_suites(&self.base, req).await
    }

    async fn rerequest_check_suite(
        &self,
        req: Request<RerequestCheckSuiteRequest>,
    ) -> Reply<CheckSuite> {
        CheckService::rerequest_check_suite(&self.base, req).await
    }

    async fn create_commit_status(
        &self,
        req: Request<CreateCommitStatusRequest>,
    ) -> Reply<CommitStatus> {
        CheckService::create_commit_status(&self.base, req).await
    }

    async fn list_commit_statuses(
        &self,
        req: Request<ListCommitStatusesRequest>,
    ) -> Reply<ListCommitStatusesResponse> {
        CheckService::list_commit_statuses(&self.base, req).await
    }

    async fn get_combined_status(
        &self,
        req: Request<GetCombinedStatusRequest>,
    ) -> Reply<CombinedStatus> {
        CheckService::get_combined_status(&self.base, req).await
    }

    async fn start_bisect(&self, req: Request<StartBisectRequest>) -> Reply<Bisect> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let auth = self
                .base
                .forge()
                .authz
                .repo(&caller, &repo, Action::Commit, &[], None, None)
                .await;
            let cx = self
                .base
                .guarded(&caller, &repo.org_id, &repo.id, "bisect.start", auth)
                .await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            Box::pin(self.bisector.start(
                &cx,
                &repo,
                StartBisect {
                    git_ref: r.r#ref.clone(),
                    check_name: r.check_name.clone(),
                    good: r.good.clone(),
                    bad: r.bad.clone(),
                    open_revert: r.open_revert,
                    manual: r.manual,
                },
            ))
            .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_bisect(&self, req: Request<GetBisectRequest>) -> Reply<Bisect> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .base
                .forge()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            self.bisector.get(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_bisects(&self, req: Request<ListBisectsRequest>) -> Reply<ListBisectsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .base
                .forge()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let state = BisectState::try_from(r.state).unwrap_or(BisectState::Unspecified);
            let (bisects, page) = paged(self.bisector.list(&cx, &repo, state).await?, &r.page);
            Ok(ListBisectsResponse { bisects, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_bisect_session(
        &self,
        req: Request<GetBisectSessionRequest>,
    ) -> Reply<BisectSession> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read_cx(&caller, &r.repo).await?;
            let s = self.bisector.session(&cx, &repo, &r.id).await?;
            Ok(BisectSession {
                bisect: Some(s.bisect),
                candidates: s.candidates,
                probes: s
                    .probes
                    .into_iter()
                    .map(|p| BisectProbe {
                        changeset: Some(p.changeset),
                        outcome: wire_outcome(p.outcome) as i32,
                        round: p.round,
                        probe_id: p.probe_id,
                        source: p.source,
                    })
                    .collect(),
                rounds: s.rounds,
                mode: s.mode,
                note: s.note,
                culprit_change_request_id: s.culprit_change_request_id,
                culprit_author: s.culprit_author,
                revert_landed: s.revert_landed,
                failing_url: s.failing_url,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_bisect_next_candidate(
        &self,
        req: Request<GetBisectNextCandidateRequest>,
    ) -> Reply<GetBisectNextCandidateResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read_cx(&caller, &r.repo).await?;
            Ok(GetBisectNextCandidateResponse {
                changeset: self.bisector.next_candidate(&cx, &repo, &r.id).await?,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn mark_bisect(&self, req: Request<MarkBisectRequest>) -> Reply<Bisect> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write_cx(&caller, &r.repo, "bisect.mark").await?;
            let outcome = match BisectOutcome::try_from(r.outcome) {
                Ok(BisectOutcome::Good) => Outcome::Pass,
                Ok(BisectOutcome::Bad) => Outcome::Fail,
                Ok(BisectOutcome::Skip) => Outcome::Skip,
                _ => return Err(ForgeError::missing("outcome")),
            };
            let cs = r
                .changeset
                .filter(|c| !c.raw.is_empty())
                .ok_or_else(|| ForgeError::missing("changeset"))?;
            Box::pin(self.bisector.mark(&cx, &repo, &r.id, &cs, outcome)).await
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_bisect(&self, req: Request<CancelBisectRequest>) -> Reply<Bisect> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write_cx(&caller, &r.repo, "bisect.cancel").await?;
            Box::pin(self.bisector.cancel(&cx, &repo, &r.id)).await
        }
        .await;
        reply(&caller, out)
    }
}

impl BisectCheckService {
    /// `read` on the repo.
    async fn read_cx(&self, caller: &Caller, r: &Option<RepoRef>) -> Result<(Repo, Cx)> {
        let repo = self.base.repo(caller, r).await?;
        let cx = self
            .base
            .forge()
            .authz
            .repo(caller, &repo, Action::Read, &[], None, None)
            .await?;
        Ok((repo, cx))
    }

    /// `commit` on the repo (denials audited; archived repos refused).
    async fn write_cx(
        &self,
        caller: &Caller,
        r: &Option<RepoRef>,
        audit: &str,
    ) -> Result<(Repo, Cx)> {
        let repo = self.base.repo(caller, r).await?;
        let auth = self
            .base
            .forge()
            .authz
            .repo(caller, &repo, Action::Commit, &[], None, None)
            .await;
        let cx = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, audit, auth)
            .await?;
        if repo.archived {
            return Err(ForgeError::archived(&repo.full_name));
        }
        Ok((repo, cx))
    }
}

fn wire_outcome(o: Option<Outcome>) -> BisectOutcome {
    match o {
        Some(Outcome::Pass) => BisectOutcome::Good,
        Some(Outcome::Fail) => BisectOutcome::Bad,
        Some(Outcome::Skip) => BisectOutcome::Skip,
        None => BisectOutcome::Unspecified,
    }
}
