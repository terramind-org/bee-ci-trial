//! The tonic `RunnerService` over [`super::Ci`].
//!
//! | RPCs | authorization |
//! |---|---|
//! | registration/removal tokens, JIT configs, runners and their labels | repo admin (`manage_settings`) or org owner/admin |
//! | runner groups, custom images | org owner/admin (reads: org members) |
//! | machine classes, images | org members |
//! | `RegisterRunner` | the registration token in the request (no bearer token) |
//! | job protocol (`AcquireRunnerJob` ... `CompleteRunnerJob`) | the runner credential (principal `runner:<id>`) |

use std::time::Duration;

use bee_proto::v1::runner_service_server::RunnerService;
use bee_proto::v1::*;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::runners::RunnerOwner;
use super::service::{CiServices, paged};
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::service::{ResponseStream, reply};

impl CiServices {
    async fn owner(
        &self,
        caller: &Caller,
        s: &Option<RunnerScope>,
        audit: &str,
    ) -> Result<(RunnerOwner, Cx)> {
        let s = s.clone().ok_or_else(|| ForgeError::missing("scope"))?;
        match RunnerScopeKind::try_from(s.kind).unwrap_or(RunnerScopeKind::Unspecified) {
            RunnerScopeKind::Repo => {
                let repo = self.base.repo(caller, &s.repo).await?;
                let cx = self.admin(caller, &repo, audit).await?;
                Ok((RunnerOwner::Repo(repo.id, repo.org_id), cx))
            }
            RunnerScopeKind::Org => {
                let (org_id, cx) = self.org(caller, &s.org, OrgPerm::Manage, audit).await?;
                Ok((RunnerOwner::Org(org_id), cx))
            }
            RunnerScopeKind::Unspecified => Err(ForgeError::missing("scope.kind")),
        }
    }

    async fn runner_caller(&self, caller: &Caller, runner_id: &str) -> Result<Cx> {
        if runner_id.is_empty() {
            return Err(ForgeError::missing("runner_id"));
        }
        let ids = vec![
            caller.token.token_id.0.clone(),
            caller.token.root_token_id.0.clone(),
        ];
        self.ci
            .authenticate_runner(&caller.cx, &caller.cx.actor_id, &ids, runner_id)
            .await?;
        Ok(caller.cx.clone())
    }
}

fn opt_ms(t: &Option<pbjson_types::Timestamp>) -> u64 {
    t.as_ref().map(ts_ms).unwrap_or(0)
}

#[tonic::async_trait]
impl RunnerService for CiServices {
    async fn create_runner_registration_token(
        &self,
        req: Request<CreateRunnerRegistrationTokenRequest>,
    ) -> std::result::Result<Response<RunnerToken>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .owner(&caller, &r.scope, "runner.register_token_create")
                .await?;
            let (token, exp) = self.ci.runner_token(&cx, &owner, "register").await?;
            Ok(RunnerToken {
                token,
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_runner_remove_token(
        &self,
        req: Request<CreateRunnerRemoveTokenRequest>,
    ) -> std::result::Result<Response<RunnerToken>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self
                .owner(&caller, &r.scope, "runner.remove_token_create")
                .await?;
            let (token, exp) = self.ci.runner_token(&cx, &owner, "remove").await?;
            Ok(RunnerToken {
                token,
                expires_at: Some(ts(exp)),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_runner_jit_config(
        &self,
        req: Request<CreateRunnerJitConfigRequest>,
    ) -> std::result::Result<Response<CreateRunnerJitConfigResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self.owner(&caller, &r.scope, "runner.register").await?;
            let (runner, encoded_jit_config) = self
                .ci
                .jit_config(
                    &cx,
                    &owner,
                    &r.name,
                    &r.runner_group_id,
                    &r.labels,
                    &r.work_folder,
                )
                .await?;
            Ok(CreateRunnerJitConfigResponse {
                runner: Some(runner),
                encoded_jit_config,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn register_runner(
        &self,
        req: Request<RegisterRunnerRequest>,
    ) -> std::result::Result<Response<RegisterRunnerResponse>, Status> {
        let rid = req
            .metadata()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let r = req.into_inner();
        let mut cx = Cx::new("runner-registration", PrincipalKind::Service);
        cx.decision = "registration_token".into();
        cx.request_id = rid.clone();
        let out = self
            .ci
            .register_runner(
                &cx,
                &r.registration_token,
                &r.name,
                RunnerOs::try_from(r.os).unwrap_or(RunnerOs::Linux),
                RunnerArch::try_from(r.arch).unwrap_or(RunnerArch::X64),
                &r.labels,
                &r.runner_group_id,
                r.ephemeral,
                &r.version,
            )
            .await
            .map(|(runner, credential)| RegisterRunnerResponse {
                runner: Some(runner),
                credential,
            });
        out.map(Response::new).map_err(|e| e.to_status(&rid))
    }

    async fn list_runners(
        &self,
        req: Request<ListRunnersRequest>,
    ) -> std::result::Result<Response<ListRunnersResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self.owner(&caller, &r.scope, "runner.list").await?;
            let status = RunnerStatus::try_from(r.status).unwrap_or(RunnerStatus::Unspecified);
            let all = self
                .ci
                .list_runners(&cx, &owner, &r.runner_group_id, &r.label, status, &r.name)
                .await?;
            let (runners, page) = paged(all, &r.page);
            Ok(ListRunnersResponse { runners, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_runner(
        &self,
        req: Request<GetRunnerRequest>,
    ) -> std::result::Result<Response<Runner>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self.owner(&caller, &r.scope, "runner.get").await?;
            self.ci.get_runner(&cx, &owner, &r.runner_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_runner(
        &self,
        req: Request<DeleteRunnerRequest>,
    ) -> std::result::Result<Response<DeleteRunnerResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self.owner(&caller, &r.scope, "runner.delete").await?;
            self.ci
                .delete_runner(&cx, &owner, &r.runner_id, r.force)
                .await?;
            Ok(DeleteRunnerResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn add_runner_labels(
        &self,
        req: Request<AddRunnerLabelsRequest>,
    ) -> std::result::Result<Response<Runner>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self.owner(&caller, &r.scope, "runner.labels").await?;
            self.ci
                .edit_labels(&cx, &owner, &r.runner_id, &r.labels, None, None)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn remove_runner_label(
        &self,
        req: Request<RemoveRunnerLabelRequest>,
    ) -> std::result::Result<Response<Runner>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self.owner(&caller, &r.scope, "runner.labels").await?;
            self.ci
                .edit_labels(&cx, &owner, &r.runner_id, &[], Some(&r.label), None)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn set_runner_labels(
        &self,
        req: Request<SetRunnerLabelsRequest>,
    ) -> std::result::Result<Response<Runner>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (owner, cx) = self.owner(&caller, &r.scope, "runner.labels").await?;
            self.ci
                .edit_labels(&cx, &owner, &r.runner_id, &[], None, Some(&r.labels))
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_runner_group(
        &self,
        req: Request<CreateRunnerGroupRequest>,
    ) -> std::result::Result<Response<RunnerGroup>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "runner.group_create")
                .await?;
            let mut repo_ids = Vec::new();
            for x in &r.selected_repos {
                repo_ids.push(self.base.repo(&caller, &Some(x.clone())).await?.id);
            }
            let vis =
                RunnerGroupVisibility::try_from(r.visibility).unwrap_or(RunnerGroupVisibility::All);
            self.ci
                .create_group(
                    &cx,
                    &org_id,
                    &r.name,
                    vis,
                    repo_ids,
                    r.runner_ids,
                    r.allows_public_repos,
                    r.restricted_to_workflows,
                    r.selected_workflows,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_runner_group(
        &self,
        req: Request<GetRunnerGroupRequest>,
    ) -> std::result::Result<Response<RunnerGroup>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.org(&caller, &r.org, OrgPerm::ViewMembers, "").await?;
            self.ci.get_group(&cx, &org_id, &r.runner_group_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_runner_groups(
        &self,
        req: Request<ListRunnerGroupsRequest>,
    ) -> std::result::Result<Response<ListRunnerGroupsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.org(&caller, &r.org, OrgPerm::ViewMembers, "").await?;
            let repo = match &r.visible_to_repo {
                Some(x) if x.repo.is_some() => {
                    Some(self.base.repo(&caller, &Some(x.clone())).await?)
                }
                _ => None,
            };
            let (groups, page) = paged(
                self.ci.list_groups(&cx, &org_id, repo.as_ref()).await?,
                &r.page,
            );
            Ok(ListRunnerGroupsResponse { groups, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_runner_group(
        &self,
        req: Request<UpdateRunnerGroupRequest>,
    ) -> std::result::Result<Response<RunnerGroup>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "runner.group_update")
                .await?;
            self.ci
                .update_group(
                    &cx,
                    &org_id,
                    &r.runner_group_id,
                    r.name.clone(),
                    r.visibility
                        .and_then(|v| RunnerGroupVisibility::try_from(v).ok()),
                    r.allows_public_repos,
                    r.restricted_to_workflows,
                    r.selected_workflows.map(|w| w.workflows),
                    r.network_configuration_id.clone(),
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_runner_group(
        &self,
        req: Request<DeleteRunnerGroupRequest>,
    ) -> std::result::Result<Response<DeleteRunnerGroupResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "runner.group_delete")
                .await?;
            self.ci
                .delete_group(&cx, &org_id, &r.runner_group_id)
                .await?;
            Ok(DeleteRunnerGroupResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_runner_group_repos(
        &self,
        req: Request<ListRunnerGroupReposRequest>,
    ) -> std::result::Result<Response<ListRunnerGroupReposResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "runner.group_repos")
                .await?;
            let ids = self
                .ci
                .group_repos(&cx, &org_id, &r.runner_group_id)
                .await?;
            let refs: Vec<RepoRef> = ids
                .iter()
                .filter_map(|i| super::engine::repo_ref_of(i))
                .collect();
            let (repos, page) = paged(refs, &r.page);
            Ok(ListRunnerGroupReposResponse { repos, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn set_runner_group_repos(
        &self,
        req: Request<SetRunnerGroupReposRequest>,
    ) -> std::result::Result<Response<SetRunnerGroupReposResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "runner.group_repos")
                .await?;
            let mut ids = Vec::new();
            for x in &r.repos {
                ids.push(self.base.repo(&caller, &Some(x.clone())).await?.id);
            }
            self.ci
                .set_group_repos(&cx, &org_id, &r.runner_group_id, ids)
                .await?;
            Ok(SetRunnerGroupReposResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn set_runner_group_runners(
        &self,
        req: Request<SetRunnerGroupRunnersRequest>,
    ) -> std::result::Result<Response<SetRunnerGroupRunnersResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "runner.group_runners")
                .await?;
            self.ci
                .set_group_runners(&cx, &org_id, &r.runner_group_id, r.runner_ids)
                .await?;
            Ok(SetRunnerGroupRunnersResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_runner_machine_classes(
        &self,
        req: Request<ListRunnerMachineClassesRequest>,
    ) -> std::result::Result<Response<ListRunnerMachineClassesResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            self.org(&caller, &r.org, OrgPerm::ViewMembers, "").await?;
            let os = RunnerOs::try_from(r.os).unwrap_or(RunnerOs::Unspecified);
            let (classes, page) = paged(self.ci.machine_classes(os), &r.page);
            Ok(ListRunnerMachineClassesResponse { classes, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_runner_images(
        &self,
        req: Request<ListRunnerImagesRequest>,
    ) -> std::result::Result<Response<ListRunnerImagesResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.org(&caller, &r.org, OrgPerm::ViewMembers, "").await?;
            let src =
                RunnerImageSource::try_from(r.source).unwrap_or(RunnerImageSource::Unspecified);
            let (images, page) = paged(self.ci.runner_images(&cx, &org_id, src).await?, &r.page);
            Ok(ListRunnerImagesResponse { images, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn create_runner_custom_image(
        &self,
        req: Request<CreateRunnerCustomImageRequest>,
    ) -> std::result::Result<Response<RunnerImage>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "runner.image_create")
                .await?;
            self.ci
                .create_image(
                    &cx,
                    &org_id,
                    &r.name,
                    RunnerOs::try_from(r.os).unwrap_or(RunnerOs::Linux),
                    RunnerArch::try_from(r.arch).unwrap_or(RunnerArch::X64),
                    &r.oci_reference,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_runner_custom_image(
        &self,
        req: Request<DeleteRunnerCustomImageRequest>,
    ) -> std::result::Result<Response<DeleteRunnerCustomImageResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self
                .org(&caller, &r.org, OrgPerm::Manage, "runner.image_delete")
                .await?;
            self.ci.delete_image(&cx, &org_id, &r.image_id).await?;
            Ok(DeleteRunnerCustomImageResponse {})
        }
        .await;
        reply(&caller, out)
    }

    type AcquireRunnerJobStream = ResponseStream<RunnerJobAssignment>;

    async fn acquire_runner_job(
        &self,
        req: Request<AcquireRunnerJobRequest>,
    ) -> std::result::Result<Response<Self::AcquireRunnerJobStream>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.runner_caller(&caller, &r.runner_id).await?;
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<RunnerJobAssignment>>(4);
            let ci = self.ci.clone();
            let keepalive = Duration::from_millis(ci.config().keepalive_ms.max(10));
            tokio::spawn(async move {
                loop {
                    let wake = ci.wake_signal().clone();
                    let notified = wake.notified();
                    let cx = cx.clone().with_timeout(Duration::from_secs(10));
                    match ci.claim(&cx, &r.runner_id, &r.labels, r.max_jobs).await {
                        Ok(Some(a)) => {
                            if tx.send(Ok(a)).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    }
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep(keepalive) => {
                            if tx.send(Ok(RunnerJobAssignment::default())).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
            let rid = caller.cx.request_id.clone();
            let stream =
                ReceiverStream::new(rx).map(move |item| item.map_err(|e| e.to_status(&rid)));
            Ok(Box::pin(stream) as ResponseStream<RunnerJobAssignment>)
        }
        .await;
        reply(&caller, out)
    }

    async fn renew_runner_job(
        &self,
        req: Request<RenewRunnerJobRequest>,
    ) -> std::result::Result<Response<RenewRunnerJobResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.runner_caller(&caller, &r.runner_id).await?;
            let (exp, cancel, force) = self
                .ci
                .renew(&cx, &r.runner_id, &r.job_id, &r.lease_id)
                .await?;
            Ok(RenewRunnerJobResponse {
                lease_expires_at: Some(ts(exp)),
                cancel_requested: cancel,
                force_cancel: force,
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_runner_job_step(
        &self,
        req: Request<UpdateRunnerJobStepRequest>,
    ) -> std::result::Result<Response<UpdateRunnerJobStepResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.runner_caller(&caller, &r.runner_id).await?;
            self.ci
                .update_step(
                    &cx,
                    &r.runner_id,
                    &r.job_id,
                    &r.lease_id,
                    r.step_number,
                    &r.step_name,
                    WorkflowStatus::try_from(r.status).unwrap_or(WorkflowStatus::Unspecified),
                    WorkflowConclusion::try_from(r.conclusion)
                        .unwrap_or(WorkflowConclusion::Unspecified),
                    opt_ms(&r.started_at),
                    opt_ms(&r.completed_at),
                )
                .await?;
            Ok(UpdateRunnerJobStepResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn append_runner_job_log(
        &self,
        req: Request<AppendRunnerJobLogRequest>,
    ) -> std::result::Result<Response<AppendRunnerJobLogResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.runner_caller(&caller, &r.runner_id).await?;
            let stored_end = self
                .ci
                .append_log(
                    &cx,
                    &r.runner_id,
                    &r.job_id,
                    &r.lease_id,
                    r.offset,
                    &r.data,
                    r.step_number,
                )
                .await?;
            Ok(AppendRunnerJobLogResponse { stored_end })
        }
        .await;
        reply(&caller, out)
    }

    async fn complete_runner_job(
        &self,
        req: Request<CompleteRunnerJobRequest>,
    ) -> std::result::Result<Response<CompleteRunnerJobResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.runner_caller(&caller, &r.runner_id).await?;
            self.ci
                .complete(
                    &cx,
                    &r.runner_id,
                    &r.job_id,
                    &r.lease_id,
                    WorkflowConclusion::try_from(r.conclusion)
                        .unwrap_or(WorkflowConclusion::Unspecified),
                    r.outputs,
                    &r.summary_markdown,
                    &r.environment_url,
                )
                .await?;
            Ok(CompleteRunnerJobResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn fetch_runner_job_secrets(
        &self,
        req: Request<FetchRunnerJobSecretsRequest>,
    ) -> std::result::Result<Response<FetchRunnerJobSecretsResponse>, Status> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let cx = self.runner_caller(&caller, &r.runner_id).await?;
            let secrets = self
                .ci
                .fetch_job_secrets(&cx, &r.runner_id, &r.job_id, &r.lease_id)
                .await?
                .into_iter()
                .map(|(key, value)| KeyValue { key, value })
                .collect();
            Ok(FetchRunnerJobSecretsResponse { secrets })
        }
        .await;
        reply(&caller, out)
    }
}
