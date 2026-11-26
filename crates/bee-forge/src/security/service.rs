//! The tonic `SecurityService` over [`Security`], following the forge service shape
//! (`docs/spec/forge-core.md` §7): authenticate, resolve the repo, authorize (denials of
//! state-changing calls are audited), refuse writes to archived repos, call the store.
//!
//! | RPCs | needs |
//! |---|---|
//! | dependency graph, manifests, SBOM, dependents, dependency review, security configuration (read), signature verification | `read` |
//! | advisory database (`ListAdvisories`, `GetAdvisory`) | any authenticated principal |
//! | snapshot submission, dependency and code scanning alerts and uploads, update config and run reads, code quality, bypass requests and reads | `commit` (write role) |
//! | secret scanning alerts and locations, update config writes, update runs, default setup writes, repo advisory writes, CVE requests, advisory collaborators | `manage_settings` (maintain role) |
//! | repo secret patterns, validity checks, publish, private reporting switch, security configuration writes | `manage_settings`, admin |
//! | bypass review | repo admin, org security manager (or org owner and admin), or a reviewer named in the repo's bypass policy; never the requester |
//! | org patterns, validity checks, overview, campaigns | org owner, admin or security manager |
//! | private vulnerability reports | `read` (an outsider who can see the repo) |
//!
//! An org security manager (or owner or admin) passes every repo level above without a repo
//! role, for alerts, bypass reviews and advisories.

use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::security_service_server::{SecurityService, SecurityServiceServer};
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::Security;
use super::repo_advisories::Viewer;
use super::secrets::PatternOwner;
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::forge::Forge;
use crate::service::{ForgeServices, next, page, reply};

type Res<T> = std::result::Result<Response<T>, Status>;

/// How much a repo caller must hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Read,
    Write,
    Maintain,
    Admin,
}

/// `SecurityService` over a forge and its security module.
#[derive(Clone)]
pub struct SecurityServices {
    base: ForgeServices,
    sec: Arc<Security>,
}

impl SecurityServices {
    /// The service over `forge` and `security`.
    pub fn new(forge: Arc<Forge>, security: Arc<Security>) -> SecurityServices {
        SecurityServices {
            base: ForgeServices::new(forge),
            sec: security,
        }
    }

    /// The security module.
    pub fn security(&self) -> &Arc<Security> {
        &self.sec
    }

    /// The tonic server.
    pub fn security_service(&self) -> SecurityServiceServer<SecurityServices> {
        SecurityServiceServer::new(self.clone())
    }

    /// Adds `SecurityService` to a tonic router builder.
    pub fn register(&self, routes: &mut tonic::service::RoutesBuilder) {
        routes.add_service(self.security_service());
    }

    fn forge(&self) -> &Arc<Forge> {
        self.base.forge()
    }

    /// Repo authorization at `level`; an org security manager passes when the repo role does
    /// not. `audit` names the action recorded when a state-changing call is denied.
    async fn authorize(
        &self,
        caller: &Caller,
        repo: &Repo,
        level: Level,
        audit: Option<&str>,
    ) -> Result<Cx> {
        let (action, min_role) = match level {
            Level::Read => (Action::Read, None),
            Level::Write => (Action::Commit, None),
            Level::Maintain => (Action::ManageSettings, None),
            Level::Admin => (Action::ManageSettings, Some(RepoRole::Admin)),
        };
        let mut auth = self
            .forge()
            .authz
            .repo(caller, repo, action, &[], None, min_role)
            .await;
        if let Err(e) = &auth
            && e.reason == ErrorReason::PermissionDenied
            && let Ok(cx) = self
                .forge()
                .authz
                .org(caller, &repo.org_id, OrgPerm::ReadAudit)
                .await
        {
            auth = Ok(cx);
        }
        match audit {
            Some(a) => {
                let cx = self
                    .base
                    .guarded(caller, &repo.org_id, &repo.id, a, auth)
                    .await?;
                if repo.archived {
                    return Err(ForgeError::archived(&repo.full_name));
                }
                Ok(cx)
            }
            None => auth,
        }
    }

    async fn viewer(&self, caller: &Caller, repo: &Repo) -> Viewer {
        let privileged = self
            .authorize(caller, repo, Level::Maintain, None)
            .await
            .is_ok();
        Viewer {
            ids: caller.cx.chain.clone(),
            privileged,
        }
    }

    async fn org_cx(&self, caller: &Caller, org_id: &str, audit: Option<&str>) -> Result<Cx> {
        let auth = self
            .forge()
            .authz
            .org(caller, org_id, OrgPerm::ReadAudit)
            .await;
        match audit {
            Some(a) => self.base.guarded(caller, org_id, "", a, auth).await,
            None => auth,
        }
    }

    /// The reviewer check of a bypass: repo admin, org security manager, or a listed reviewer.
    async fn reviewer_cx(&self, caller: &Caller, repo: &Repo) -> Result<Cx> {
        let admin = self.authorize(caller, repo, Level::Admin, None).await;
        let auth = match admin {
            Ok(cx) => Ok(cx),
            Err(e) => {
                let read = self.authorize(caller, repo, Level::Read, None).await;
                match read {
                    Ok(cx)
                        if self
                            .sec
                            .is_listed_reviewer(&cx, repo, &caller.cx.actor_id)
                            .await? =>
                    {
                        Ok(cx)
                    }
                    _ => Err(e),
                }
            }
        };
        self.base
            .guarded(
                caller,
                &repo.org_id,
                &repo.id,
                "push_protection.bypass_review",
                auth,
            )
            .await
    }

    async fn pattern_scope(
        &self,
        caller: &Caller,
        pattern_id: &str,
        audit: &str,
    ) -> Result<(SecretScanningPattern, String, Cx)> {
        let (p, scope) = self.sec.pattern_by_id(&caller.cx, pattern_id).await?;
        let cx = if p.repo.is_some() {
            let repo = self.forge().orgs.repo_any(&caller.cx, &scope).await?;
            self.authorize(caller, &repo, Level::Admin, Some(audit))
                .await
        } else {
            self.org_cx(caller, &scope, Some(audit)).await
        };
        match cx {
            Ok(cx) => Ok((p, scope, cx)),
            Err(e) if e.reason == ErrorReason::PermissionDenied => {
                Err(ForgeError::not_found(format!("pattern {pattern_id}")))
            }
            Err(e) => Err(e),
        }
    }
}

#[tonic::async_trait]
impl SecurityService for SecurityServices {
    // ---- dependency graph ---------------------------------------------------------------

    async fn list_dependencies(
        &self,
        req: Request<ListDependenciesRequest>,
    ) -> Res<ListDependenciesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            let (token, size) = page(&r.page);
            self.sec
                .list_dependencies(&cx, &repo, &r, &token, size)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_dependency_manifests(
        &self,
        req: Request<ListDependencyManifestsRequest>,
    ) -> Res<ListDependencyManifestsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            let (token, size) = page(&r.page);
            let rev = r.revision.map(|x| x.expr).unwrap_or_default();
            let (manifests, nt) = self
                .sec
                .list_manifests(&cx, &repo, &rev, &token, size)
                .await?;
            Ok(ListDependencyManifestsResponse {
                manifests,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn submit_dependency_snapshot(
        &self,
        req: Request<SubmitDependencySnapshotRequest>,
    ) -> Res<SubmitDependencySnapshotResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Write,
                    Some("security.dependency_snapshot.submit"),
                )
                .await?;
            self.sec.submit_snapshot(&cx, &repo, r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_dependents(
        &self,
        req: Request<ListDependentsRequest>,
    ) -> Res<ListDependentsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            let (token, size) = page(&r.page);
            let (dependents, total, nt) = self
                .sec
                .list_dependents(&cx, &repo, &r, &token, size)
                .await?;
            Ok(ListDependentsResponse {
                dependents,
                total,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    // ---- SBOM ---------------------------------------------------------------------------

    async fn export_sbom(&self, req: Request<ExportSbomRequest>) -> Res<ExportSbomResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            self.sec.export_sbom(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    // ---- global advisory database -----------------------------------------------------------

    async fn list_advisories(
        &self,
        req: Request<ListAdvisoriesRequest>,
    ) -> Res<ListAdvisoriesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (token, size) = page(&r.page);
            let (advisories, nt) = self
                .sec
                .list_advisories(&caller.cx, &r, &token, size)
                .await?;
            Ok(ListAdvisoriesResponse {
                advisories,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_advisory(&self, req: Request<GetAdvisoryRequest>) -> Res<Advisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = self.sec.get_advisory(&caller.cx, &r).await;
        reply(&caller, out)
    }

    // ---- vulnerability alerts -----------------------------------------------------------------

    async fn list_dependency_alerts(
        &self,
        req: Request<ListDependencyAlertsRequest>,
    ) -> Res<ListDependencyAlertsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            let (token, size) = page(&r.page);
            let (alerts, nt) = self
                .sec
                .list_dependency_alerts(&cx, &repo, &r, &token, size)
                .await?;
            Ok(ListDependencyAlertsResponse {
                alerts,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_dependency_alert(
        &self,
        req: Request<GetDependencyAlertRequest>,
    ) -> Res<DependencyAlert> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            self.sec.get_dependency_alert(&cx, &repo, r.number).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_dependency_alert(
        &self,
        req: Request<UpdateDependencyAlertRequest>,
    ) -> Res<DependencyAlert> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Write,
                    Some("security.dependency_alert.update"),
                )
                .await?;
            self.sec.update_dependency_alert(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    // ---- automated updates ------------------------------------------------------------------

    async fn get_dependency_update_config(
        &self,
        req: Request<GetDependencyUpdateConfigRequest>,
    ) -> Res<DependencyUpdateConfig> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            self.sec.get_update_config(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_dependency_update_config(
        &self,
        req: Request<SetDependencyUpdateConfigRequest>,
    ) -> Res<DependencyUpdateConfig> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Maintain,
                    Some("security.dependency_update.config"),
                )
                .await?;
            self.sec.set_update_config(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn trigger_dependency_update_run(
        &self,
        req: Request<TriggerDependencyUpdateRunRequest>,
    ) -> Res<DependencyUpdateRun> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Maintain,
                    Some("security.dependency_update.run"),
                )
                .await?;
            self.sec
                .trigger_run(&cx, &repo, &r.ecosystem, &r.directory, r.security_only)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_dependency_update_runs(
        &self,
        req: Request<ListDependencyUpdateRunsRequest>,
    ) -> Res<ListDependencyUpdateRunsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            let (token, size) = page(&r.page);
            let (runs, nt) = self
                .sec
                .list_runs(&cx, &repo, r.state, &token, size)
                .await?;
            Ok(ListDependencyUpdateRunsResponse {
                runs,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_dependency_update_run(
        &self,
        req: Request<GetDependencyUpdateRunRequest>,
    ) -> Res<DependencyUpdateRun> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            self.sec.get_run(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    // ---- dependency review -------------------------------------------------------------------

    async fn get_dependency_review(
        &self,
        req: Request<GetDependencyReviewRequest>,
    ) -> Res<DependencyReview> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            self.sec.dependency_review(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    // ---- code scanning ------------------------------------------------------------------------

    async fn upload_sarif(&self, req: Request<UploadSarifRequest>) -> Res<CodeScanningSarifUpload> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Write,
                    Some("security.code_scanning.upload"),
                )
                .await?;
            self.sec.upload_sarif(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_sarif_upload(
        &self,
        req: Request<GetSarifUploadRequest>,
    ) -> Res<CodeScanningSarifUpload> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            self.sec.get_sarif_upload(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_code_scanning_alerts(
        &self,
        req: Request<ListCodeScanningAlertsRequest>,
    ) -> Res<ListCodeScanningAlertsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            let (token, size) = page(&r.page);
            let (alerts, nt) = self
                .sec
                .list_code_alerts(&cx, &repo, &r, &token, size)
                .await?;
            Ok(ListCodeScanningAlertsResponse {
                alerts,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_code_scanning_alert(
        &self,
        req: Request<GetCodeScanningAlertRequest>,
    ) -> Res<CodeScanningAlert> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            self.sec.get_code_alert(&cx, &repo, r.number).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_code_scanning_alert(
        &self,
        req: Request<UpdateCodeScanningAlertRequest>,
    ) -> Res<CodeScanningAlert> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Write,
                    Some("security.code_alert.update"),
                )
                .await?;
            self.sec.update_code_alert(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_code_scanning_alert_instances(
        &self,
        req: Request<ListCodeScanningAlertInstancesRequest>,
    ) -> Res<ListCodeScanningAlertInstancesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            let (token, size) = page(&r.page);
            let (instances, nt) = self
                .sec
                .list_code_instances(&cx, &repo, r.number, &r.r#ref, &token, size)
                .await?;
            Ok(ListCodeScanningAlertInstancesResponse {
                instances,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_code_scanning_analyses(
        &self,
        req: Request<ListCodeScanningAnalysesRequest>,
    ) -> Res<ListCodeScanningAnalysesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            let (token, size) = page(&r.page);
            let (analyses, nt) = self
                .sec
                .list_analyses(
                    &cx,
                    &repo,
                    &r.r#ref,
                    &r.tool_name,
                    r.changeset.as_ref(),
                    &token,
                    size,
                )
                .await?;
            Ok(ListCodeScanningAnalysesResponse {
                analyses,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_code_scanning_analysis(
        &self,
        req: Request<GetCodeScanningAnalysisRequest>,
    ) -> Res<CodeScanningAnalysis> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            self.sec.get_analysis(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_code_scanning_default_setup(
        &self,
        req: Request<GetCodeScanningDefaultSetupRequest>,
    ) -> Res<CodeScanningDefaultSetup> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            self.sec.get_default_setup(&cx, &repo).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_code_scanning_default_setup(
        &self,
        req: Request<UpdateCodeScanningDefaultSetupRequest>,
    ) -> Res<CodeScanningDefaultSetup> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Maintain,
                    Some("security.code_scanning.setup"),
                )
                .await?;
            self.sec.update_default_setup(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn request_code_scanning_autofix(
        &self,
        req: Request<RequestCodeScanningAutofixRequest>,
    ) -> Res<RequestCodeScanningAutofixResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Write,
                    Some("security.code_alert.autofix"),
                )
                .await?;
            let agent_run_id = self
                .sec
                .request_autofix(&cx, &repo, r.number, &r.agent_runtime_id)
                .await?;
            Ok(RequestCodeScanningAutofixResponse { agent_run_id })
        }
        .await;
        reply(&caller, out)
    }

    // ---- secret scanning --------------------------------------------------------------------------

    async fn list_secret_scanning_alerts(
        &self,
        req: Request<ListSecretScanningAlertsRequest>,
    ) -> Res<ListSecretScanningAlertsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(&caller, &repo, Level::Maintain, None)
                .await?;
            let (token, size) = page(&r.page);
            let (alerts, nt) = self
                .sec
                .list_secret_alerts(&cx, &repo, &r, &token, size)
                .await?;
            Ok(ListSecretScanningAlertsResponse {
                alerts,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_secret_scanning_alert(
        &self,
        req: Request<GetSecretScanningAlertRequest>,
    ) -> Res<SecretScanningAlert> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(&caller, &repo, Level::Maintain, None)
                .await?;
            self.sec.get_secret_alert(&cx, &repo, r.number).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_secret_scanning_alert(
        &self,
        req: Request<UpdateSecretScanningAlertRequest>,
    ) -> Res<SecretScanningAlert> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Maintain,
                    Some("security.secret_alert.update"),
                )
                .await?;
            self.sec.update_secret_alert(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_secret_scanning_locations(
        &self,
        req: Request<ListSecretScanningLocationsRequest>,
    ) -> Res<ListSecretScanningLocationsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(&caller, &repo, Level::Maintain, None)
                .await?;
            let (token, size) = page(&r.page);
            let (locations, nt) = self
                .sec
                .secret_locations(&cx, &repo, r.number, &token, size)
                .await?;
            Ok(ListSecretScanningLocationsResponse {
                locations,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_secret_scanning_pattern(
        &self,
        req: Request<CreateSecretScanningPatternRequest>,
    ) -> Res<SecretScanningPattern> {
        use bee_proto::v1::create_secret_scanning_pattern_request::Scope;
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            match &r.scope {
                Some(Scope::Repo(rr)) => {
                    let repo = self.base.repo(&caller, &Some(rr.clone())).await?;
                    let cx = self
                        .authorize(
                            &caller,
                            &repo,
                            Level::Admin,
                            Some("security.secret_pattern.create"),
                        )
                        .await?;
                    self.sec
                        .create_pattern(&cx, PatternOwner::Repo(&repo), &r)
                        .await
                }
                Some(Scope::Org(name)) => {
                    let org = self.forge().orgs.get_org(&caller.cx, name).await?;
                    let cx = self
                        .org_cx(&caller, &org.id, Some("security.secret_pattern.create"))
                        .await?;
                    self.sec
                        .create_pattern(&cx, PatternOwner::Org(&org.id, &org.name), &r)
                        .await
                }
                None => Err(ForgeError::missing("org or repo")),
            }
        }
        .await;
        reply(&caller, out)
    }

    async fn list_secret_scanning_patterns(
        &self,
        req: Request<ListSecretScanningPatternsRequest>,
    ) -> Res<ListSecretScanningPatternsResponse> {
        use bee_proto::v1::list_secret_scanning_patterns_request::Scope;
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (token, size) = page(&r.page);
            let (patterns, nt) = match &r.scope {
                Some(Scope::Repo(rr)) => {
                    let repo = self.base.repo(&caller, &Some(rr.clone())).await?;
                    let cx = self.authorize(&caller, &repo, Level::Admin, None).await?;
                    self.sec.list_patterns(&cx, &repo.id, &token, size).await?
                }
                Some(Scope::Org(name)) => {
                    let org = self.forge().orgs.get_org(&caller.cx, name).await?;
                    let cx = self.org_cx(&caller, &org.id, None).await?;
                    self.sec.list_patterns(&cx, &org.id, &token, size).await?
                }
                None => return Err(ForgeError::missing("org or repo")),
            };
            Ok(ListSecretScanningPatternsResponse {
                patterns,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_secret_scanning_pattern(
        &self,
        req: Request<UpdateSecretScanningPatternRequest>,
    ) -> Res<SecretScanningPattern> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, _, cx) = self
                .pattern_scope(&caller, &r.id, "security.secret_pattern.update")
                .await?;
            self.sec.update_pattern(&cx, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_secret_scanning_pattern(
        &self,
        req: Request<DeleteSecretScanningPatternRequest>,
    ) -> Res<DeleteSecretScanningPatternResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (_, _, cx) = self
                .pattern_scope(&caller, &r.id, "security.secret_pattern.delete")
                .await?;
            self.sec.delete_pattern(&cx, &r.id).await?;
            Ok(DeleteSecretScanningPatternResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn set_secret_scanning_validity_checks(
        &self,
        req: Request<SetSecretScanningValidityChecksRequest>,
    ) -> Res<SetSecretScanningValidityChecksResponse> {
        use bee_proto::v1::set_secret_scanning_validity_checks_request::Scope;
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let enabled = match &r.scope {
                Some(Scope::Repo(rr)) => {
                    let repo = self.base.repo(&caller, &Some(rr.clone())).await?;
                    let cx = self
                        .authorize(&caller, &repo, Level::Admin, Some("security.config.update"))
                        .await?;
                    self.sec
                        .update_config(
                            &cx,
                            &repo,
                            &UpdateSecurityConfigurationRequest {
                                secret_scanning_validity_checks: Some(r.enabled),
                                ..Default::default()
                            },
                        )
                        .await?
                        .secret_scanning_validity_checks
                }
                Some(Scope::Org(name)) => {
                    let org = self.forge().orgs.get_org(&caller.cx, name).await?;
                    let cx = self
                        .org_cx(&caller, &org.id, Some("security.validity_checks.update"))
                        .await?;
                    self.sec
                        .set_org_validity_checks(&cx, &org.id, r.enabled)
                        .await?
                }
                None => return Err(ForgeError::missing("org or repo")),
            };
            Ok(SetSecretScanningValidityChecksResponse { enabled })
        }
        .await;
        reply(&caller, out)
    }

    // ---- push protection ----------------------------------------------------------------------------

    async fn request_push_protection_bypass(
        &self,
        req: Request<RequestPushProtectionBypassRequest>,
    ) -> Res<PushProtectionBypass> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Write,
                    Some("push_protection.bypass_request"),
                )
                .await?;
            self.sec.request_bypass(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_push_protection_bypasses(
        &self,
        req: Request<ListPushProtectionBypassesRequest>,
    ) -> Res<ListPushProtectionBypassesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            let (token, size) = page(&r.page);
            let (bypasses, nt) = self
                .sec
                .list_bypasses(&cx, &repo, r.state, &r.requester_id, &token, size)
                .await?;
            Ok(ListPushProtectionBypassesResponse {
                bypasses,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_push_protection_bypass(
        &self,
        req: Request<GetPushProtectionBypassRequest>,
    ) -> Res<PushProtectionBypass> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            self.sec.get_bypass(&cx, &repo, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn review_push_protection_bypass(
        &self,
        req: Request<ReviewPushProtectionBypassRequest>,
    ) -> Res<PushProtectionBypass> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            let cx = self.reviewer_cx(&caller, &repo).await?;
            self.sec.review_bypass(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    // ---- repository security advisories ----------------------------------------------------------------

    async fn create_security_advisory(
        &self,
        req: Request<CreateSecurityAdvisoryRequest>,
    ) -> Res<SecurityRepoAdvisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Maintain,
                    Some("security.advisory.create"),
                )
                .await?;
            self.sec.create_advisory(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_security_advisories(
        &self,
        req: Request<ListSecurityAdvisoriesRequest>,
    ) -> Res<ListSecurityAdvisoriesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            let viewer = self.viewer(&caller, &repo).await;
            let (token, size) = page(&r.page);
            let (advisories, nt) = self
                .sec
                .list_repo_advisories(&cx, &repo, r.state, &viewer, &token, size)
                .await?;
            Ok(ListSecurityAdvisoriesResponse {
                advisories,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_security_advisory(
        &self,
        req: Request<GetSecurityAdvisoryRequest>,
    ) -> Res<SecurityRepoAdvisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            let viewer = self.viewer(&caller, &repo).await;
            self.sec
                .get_repo_advisory(&cx, &repo, &r.ghsa_id, &viewer)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_security_advisory(
        &self,
        req: Request<UpdateSecurityAdvisoryRequest>,
    ) -> Res<SecurityRepoAdvisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            let viewer = self.viewer(&caller, &repo).await;
            let res = self.sec.update_repo_advisory(&cx, &repo, &r, &viewer).await;
            if let Err(e) = &res {
                self.forge()
                    .authz
                    .audit_denial(
                        &caller,
                        &repo.org_id,
                        &repo.id,
                        "security.advisory.update",
                        e,
                    )
                    .await;
            }
            res
        }
        .await;
        reply(&caller, out)
    }

    async fn publish_security_advisory(
        &self,
        req: Request<PublishSecurityAdvisoryRequest>,
    ) -> Res<SecurityRepoAdvisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Admin,
                    Some("security.advisory.publish"),
                )
                .await?;
            let viewer = Viewer {
                ids: caller.cx.chain.clone(),
                privileged: true,
            };
            self.sec
                .publish_advisory(&cx, &repo, &r.ghsa_id, &viewer)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn request_advisory_cve(
        &self,
        req: Request<RequestAdvisoryCveRequest>,
    ) -> Res<SecurityRepoAdvisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Maintain,
                    Some("security.advisory.cve_request"),
                )
                .await?;
            let viewer = Viewer {
                ids: caller.cx.chain.clone(),
                privileged: true,
            };
            self.sec.request_cve(&cx, &repo, &r.ghsa_id, &viewer).await
        }
        .await;
        reply(&caller, out)
    }

    async fn add_security_advisory_collaborator(
        &self,
        req: Request<AddSecurityAdvisoryCollaboratorRequest>,
    ) -> Res<SecurityRepoAdvisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Maintain,
                    Some("security.advisory.collaborator_add"),
                )
                .await?;
            let viewer = Viewer {
                ids: caller.cx.chain.clone(),
                privileged: true,
            };
            self.sec
                .add_advisory_collaborator(&cx, &repo, &r, &viewer)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_security_advisory_collaborator(
        &self,
        req: Request<RemoveSecurityAdvisoryCollaboratorRequest>,
    ) -> Res<SecurityRepoAdvisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Maintain,
                    Some("security.advisory.collaborator_remove"),
                )
                .await?;
            let viewer = Viewer {
                ids: caller.cx.chain.clone(),
                privileged: true,
            };
            self.sec
                .remove_advisory_collaborator(&cx, &repo, &r, &viewer)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_private_vulnerability_reporting(
        &self,
        req: Request<GetPrivateVulnerabilityReportingRequest>,
    ) -> Res<GetPrivateVulnerabilityReportingResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            let enabled = self
                .sec
                .config_of(&cx, &repo)
                .await?
                .private_vulnerability_reporting;
            Ok(GetPrivateVulnerabilityReportingResponse { enabled })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_private_vulnerability_reporting(
        &self,
        req: Request<SetPrivateVulnerabilityReportingRequest>,
    ) -> Res<SetPrivateVulnerabilityReportingResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(&caller, &repo, Level::Admin, Some("security.config.update"))
                .await?;
            let cfg = self
                .sec
                .update_config(
                    &cx,
                    &repo,
                    &UpdateSecurityConfigurationRequest {
                        private_vulnerability_reporting: Some(r.enabled),
                        ..Default::default()
                    },
                )
                .await?;
            Ok(SetPrivateVulnerabilityReportingResponse {
                enabled: cfg.private_vulnerability_reporting,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn submit_vulnerability_report(
        &self,
        req: Request<SubmitVulnerabilityReportRequest>,
    ) -> Res<SecurityRepoAdvisory> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            self.sec.submit_report(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    // ---- overview and campaigns ---------------------------------------------------------------------------

    async fn get_security_overview(
        &self,
        req: Request<GetSecurityOverviewRequest>,
    ) -> Res<SecurityOverview> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.forge().orgs.get_org(&caller.cx, &r.org).await?;
            let cx = self.org_cx(&caller, &org.id, None).await?;
            let (token, size) = page(&r.page);
            self.sec
                .overview(&cx, &org.id, &org.name, &r, &token, size)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_security_campaign(
        &self,
        req: Request<CreateSecurityCampaignRequest>,
    ) -> Res<SecurityCampaign> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.forge().orgs.get_org(&caller.cx, &r.org).await?;
            let cx = self
                .org_cx(&caller, &org.id, Some("security.campaign.create"))
                .await?;
            self.sec.create_campaign(&cx, &org.id, &org.name, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_security_campaign(
        &self,
        req: Request<GetSecurityCampaignRequest>,
    ) -> Res<SecurityCampaign> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.forge().orgs.get_org(&caller.cx, &r.org).await?;
            let cx = self.org_cx(&caller, &org.id, None).await?;
            self.sec.get_campaign(&cx, &org.id, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_security_campaigns(
        &self,
        req: Request<ListSecurityCampaignsRequest>,
    ) -> Res<ListSecurityCampaignsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.forge().orgs.get_org(&caller.cx, &r.org).await?;
            let cx = self.org_cx(&caller, &org.id, None).await?;
            let (token, size) = page(&r.page);
            let (campaigns, nt) = self
                .sec
                .list_campaigns(&cx, &org.id, r.state, &token, size)
                .await?;
            Ok(ListSecurityCampaignsResponse {
                campaigns,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_security_campaign(
        &self,
        req: Request<UpdateSecurityCampaignRequest>,
    ) -> Res<SecurityCampaign> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let org = self.forge().orgs.get_org(&caller.cx, &r.org).await?;
            let cx = self
                .org_cx(&caller, &org.id, Some("security.campaign.update"))
                .await?;
            self.sec.update_campaign(&cx, &org.id, &r).await
        }
        .await;
        reply(&caller, out)
    }

    // ---- code quality ----------------------------------------------------------------------------------------

    async fn list_code_quality_findings(
        &self,
        req: Request<ListCodeQualityFindingsRequest>,
    ) -> Res<ListCodeQualityFindingsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Write, None).await?;
            let (token, size) = page(&r.page);
            let (findings, nt) = self.sec.list_quality(&cx, &repo, &r, &token, size).await?;
            Ok(ListCodeQualityFindingsResponse {
                findings,
                page: next(nt),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_code_quality_finding(
        &self,
        req: Request<UpdateCodeQualityFindingRequest>,
    ) -> Res<SecurityCodeQualityFinding> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(
                    &caller,
                    &repo,
                    Level::Write,
                    Some("security.code_quality.update"),
                )
                .await?;
            self.sec.update_quality(&cx, &repo, &r).await
        }
        .await;
        reply(&caller, out)
    }

    // ---- signatures ----------------------------------------------------------------------------------------------

    async fn verify_object_signature(
        &self,
        req: Request<VerifyObjectSignatureRequest>,
    ) -> Res<VerifyObjectSignatureResponse> {
        use bee_proto::v1::verify_object_signature_request::Target;
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let _cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            match &r.target {
                Some(Target::Changeset(c)) => {
                    self.sec.ports().signatures.verify(&repo, Some(c), "").await
                }
                Some(Target::Tag(t)) => self.sec.ports().signatures.verify(&repo, None, t).await,
                None => Err(ForgeError::missing("changeset or tag")),
            }
        }
        .await;
        reply(&caller, out)
    }

    // ---- security configuration -----------------------------------------------------------------------------------------

    async fn get_security_configuration(
        &self,
        req: Request<GetSecurityConfigurationRequest>,
    ) -> Res<SecurityConfiguration> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.authorize(&caller, &repo, Level::Read, None).await?;
            let cfg = self.sec.config_of(&cx, &repo).await?;
            self.with_bypass(&cx, &repo, cfg).await
        }
        .await;
        reply(&caller, out)
    }

    async fn update_security_configuration(
        &self,
        req: Request<UpdateSecurityConfigurationRequest>,
    ) -> Res<SecurityConfiguration> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(&caller, &repo, Level::Admin, Some("security.config.update"))
                .await?;
            if r.push_protection_bypass_mode.is_some()
                || r.bypass_ttl_hours.is_some()
                || r.replace_bypass_reviewers
                || !r.bypass_reviewer_ids.is_empty()
            {
                let cur = self.sec.bypass_policy(&cx, &repo).await?;
                let reviewers = if r.replace_bypass_reviewers || !r.bypass_reviewer_ids.is_empty() {
                    r.bypass_reviewer_ids.clone()
                } else {
                    cur.reviewer_ids
                };
                self.sec
                    .set_bypass_policy(
                        &cx,
                        &repo,
                        r.push_protection_bypass_mode
                            .as_deref()
                            .unwrap_or(&cur.mode),
                        reviewers,
                        r.bypass_ttl_hours.unwrap_or(cur.ttl_hours),
                    )
                    .await?;
            }
            let cfg = self.sec.update_config(&cx, &repo, &r).await?;
            self.with_bypass(&cx, &repo, cfg).await
        }
        .await;
        reply(&caller, out)
    }

    async fn rescan_secrets(
        &self,
        req: Request<RescanSecretsRequest>,
    ) -> Res<RescanSecretsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .authorize(&caller, &repo, Level::Admin, Some("secret_scanning.rescan"))
                .await?;
            if !self.sec.config_of(&cx, &repo).await?.secret_scanning {
                return Err(ForgeError::invalid_state(
                    "secret scanning is off for this repo",
                ));
            }
            let tip = self.sec.ports().vcs.resolve(&repo, &r.r#ref).await?;
            let limit = if r.limit == 0 { 1000 } else { r.limit as usize };
            let opened = self.sec.scan_history(&cx, &repo, &tip, limit).await?;
            Ok(RescanSecretsResponse {
                opened_alert_numbers: opened,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_security_policy(
        &self,
        req: Request<GetSecurityPolicyRequest>,
    ) -> Res<SecurityPolicy> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            self.authorize(&caller, &repo, Level::Read, None).await?;
            let (path, content) = self
                .sec
                .security_policy(&repo)
                .await?
                .ok_or_else(|| ForgeError::not_found("security policy (SECURITY.md)"))?;
            Ok(SecurityPolicy {
                repo: r.repo.clone(),
                path: path.into_bytes(),
                content_html: crate::markdown::render_html(&content),
                content,
            })
        }
        .await;
        reply(&caller, out)
    }
}

impl SecurityServices {
    /// The configuration with the push-protection bypass policy filled in.
    async fn with_bypass(
        &self,
        cx: &Cx,
        repo: &Repo,
        mut cfg: SecurityConfiguration,
    ) -> Result<SecurityConfiguration> {
        let p = self.sec.bypass_policy(cx, repo).await?;
        cfg.push_protection_bypass_mode = p.mode;
        cfg.bypass_reviewer_ids = p.reviewer_ids;
        cfg.bypass_ttl_hours = p.ttl_hours;
        Ok(cfg)
    }
}
