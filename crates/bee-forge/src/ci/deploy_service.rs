//! The tonic `DeploymentService` over the CI environment and deployment stores
//! ([`super::environments`], [`super::deployments`]).
//!
//! | RPCs | authorization |
//! |---|---|
//! | environment, deployment, status and review reads; green status; pending reviews | `read` |
//! | create deployment, create status, delete deployment | `commit` (write role) |
//! | create, update, delete environments; add or remove custom protection rules | `manage_settings` (admin) |
//! | review a pending deployment | `read`, plus being a required reviewer (or an admin when the environment allows bypass) |
//! | report a protection rule result | `read`, and the caller is the rule's app |
//!
//! `ListDeploymentProtectionRuleApps` needs the app installations of the enterprise apps
//! module and answers `UNIMPLEMENTED` (subcode `served_elsewhere`) here.

use bee_proto::v1::deployment_service_server::DeploymentService;
use bee_proto::v1::*;
use tonic::{Request, Response, Status};

use super::deployments::{DeploymentFilter, NewDeployment, NewDeploymentStatus};
use super::environments::EnvironmentSpec;
use super::service::{CiServices, paged};
use crate::error::ForgeError;
use crate::ids::hex;
use crate::service::{reply, unserved};

type Reply<T> = std::result::Result<Response<T>, Status>;

fn mode(v: i32) -> DeploymentRefPolicyMode {
    DeploymentRefPolicyMode::try_from(v).unwrap_or(DeploymentRefPolicyMode::Unspecified)
}

fn decision(v: i32) -> DeploymentReviewDecision {
    DeploymentReviewDecision::try_from(v).unwrap_or(DeploymentReviewDecision::Unspecified)
}

#[tonic::async_trait]
impl DeploymentService for CiServices {
    async fn create_deployment_environment(
        &self,
        req: Request<CreateDeploymentEnvironmentRequest>,
    ) -> Reply<DeploymentEnvironment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.admin(&caller, &repo, "environment.create").await?;
            let m = mode(r.ref_policy_mode);
            let spec = EnvironmentSpec {
                reviewers: Some(r.reviewers.clone()),
                prevent_self_review: Some(r.prevent_self_review),
                wait_timer_seconds: Some(r.wait_timer_seconds),
                ref_policy_mode: (m != DeploymentRefPolicyMode::Unspecified).then_some(m),
                ref_policies: Some(r.ref_policies.clone()),
                can_admins_bypass: Some(r.can_admins_bypass),
                require_green: Some(r.require_green),
            };
            self.ci.create_environment(&cx, &repo, &r.name, &spec).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_deployment_environment(
        &self,
        req: Request<GetDeploymentEnvironmentRequest>,
    ) -> Reply<DeploymentEnvironment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci.get_environment(&cx, &repo, &r.name).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_deployment_environments(
        &self,
        req: Request<ListDeploymentEnvironmentsRequest>,
    ) -> Reply<ListDeploymentEnvironmentsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let mut all = self.ci.list_environments(&cx, &repo).await?;
            all.sort_by_key(|a| a.name.to_lowercase());
            let (environments, page) = paged(all, &r.page);
            Ok(ListDeploymentEnvironmentsResponse { environments, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn update_deployment_environment(
        &self,
        req: Request<UpdateDeploymentEnvironmentRequest>,
    ) -> Reply<DeploymentEnvironment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.admin(&caller, &repo, "environment.update").await?;
            let spec = EnvironmentSpec {
                reviewers: r.reviewers.clone().map(|s| s.reviewers),
                prevent_self_review: r.prevent_self_review,
                wait_timer_seconds: r.wait_timer_seconds,
                ref_policy_mode: r
                    .ref_policy_mode
                    .map(mode)
                    .filter(|m| *m != DeploymentRefPolicyMode::Unspecified),
                ref_policies: r.ref_policies.clone().map(|s| s.policies),
                can_admins_bypass: r.can_admins_bypass,
                require_green: r.require_green,
            };
            self.ci.update_environment(&cx, &repo, &r.name, &spec).await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_deployment_environment(
        &self,
        req: Request<DeleteDeploymentEnvironmentRequest>,
    ) -> Reply<DeleteDeploymentEnvironmentResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self.admin(&caller, &repo, "environment.delete").await?;
            self.ci.delete_environment(&cx, &repo, &r.name).await?;
            Ok(DeleteDeploymentEnvironmentResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn list_deployment_protection_rule_apps(
        &self,
        req: Request<ListDeploymentProtectionRuleAppsRequest>,
    ) -> Reply<ListDeploymentProtectionRuleAppsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            self.read(&caller, &r.repo).await?;
            Err(unserved(
                "DeploymentService.ListDeploymentProtectionRuleApps",
                "enterprise apps (installations)",
            ))
        }
        .await;
        reply(&caller, out)
    }

    async fn create_deployment_protection_rule(
        &self,
        req: Request<CreateDeploymentProtectionRuleRequest>,
    ) -> Reply<DeploymentProtectionRule> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .admin(&caller, &repo, "environment.protection_rule_add")
                .await?;
            self.ci
                .add_protection_rule(&cx, &repo, &r.environment, &r.app_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_deployment_protection_rule(
        &self,
        req: Request<DeleteDeploymentProtectionRuleRequest>,
    ) -> Reply<DeleteDeploymentProtectionRuleResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.base.repo(&caller, &r.repo).await?;
            let cx = self
                .admin(&caller, &repo, "environment.protection_rule_remove")
                .await?;
            self.ci
                .remove_protection_rule(&cx, &repo, &r.environment, &r.rule_id)
                .await?;
            Ok(DeleteDeploymentProtectionRuleResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn report_deployment_protection_rule_result(
        &self,
        req: Request<ReportDeploymentProtectionRuleResultRequest>,
    ) -> Reply<ReportDeploymentProtectionRuleResultResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let res = self
                .ci
                .report_protection_rule(
                    &cx,
                    &repo,
                    &r.deployment_id,
                    &r.environment,
                    decision(r.decision),
                    &r.comment,
                )
                .await;
            if let Err(e) = &res {
                self.forge()
                    .authz
                    .audit_denial(
                        &caller,
                        &repo.org_id,
                        &repo.id,
                        "deployment.protection_rule_result",
                        e,
                    )
                    .await;
            }
            res?;
            Ok(ReportDeploymentProtectionRuleResultResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn create_deployment(&self, req: Request<CreateDeploymentRequest>) -> Reply<Deployment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "deployment.create").await?;
            self.ci
                .create_deployment(
                    &cx,
                    &repo,
                    NewDeployment {
                        git_ref: r.r#ref.clone(),
                        changeset: r.changeset.clone(),
                        task: r.task.clone(),
                        environment: r.environment.clone(),
                        description: r.description.clone(),
                        payload_json: r.payload_json.clone(),
                        transient: r.transient_environment,
                        production: r.production_environment,
                        auto_merge: r.auto_merge,
                        required_contexts: r.required_contexts.clone().map(|c| c.contexts),
                        deploy_green_tip: r.deploy_green_tip,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_deployment(&self, req: Request<GetDeploymentRequest>) -> Reply<Deployment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci.get_deployment(&cx, &repo, &r.deployment_id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_deployments(
        &self,
        req: Request<ListDeploymentsRequest>,
    ) -> Reply<ListDeploymentsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let f = DeploymentFilter {
                environment: r.environment.clone(),
                git_ref: r.r#ref.clone(),
                changeset_hex: r
                    .changeset
                    .as_ref()
                    .map(|c| hex(&c.raw))
                    .unwrap_or_default(),
                task: r.task.clone(),
                state: DeploymentState::try_from(r.state).unwrap_or(DeploymentState::Unspecified),
                creator_id: r.creator_id.clone(),
            };
            let (deployments, page) =
                paged(self.ci.find_deployments(&cx, &repo, &f).await?, &r.page);
            Ok(ListDeploymentsResponse { deployments, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_deployment(
        &self,
        req: Request<DeleteDeploymentRequest>,
    ) -> Reply<DeleteDeploymentResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "deployment.delete").await?;
            self.ci
                .delete_deployment(&cx, &repo, &r.deployment_id)
                .await?;
            Ok(DeleteDeploymentResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn create_deployment_status(
        &self,
        req: Request<CreateDeploymentStatusRequest>,
    ) -> Reply<DeploymentStatus> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.write(&caller, &r.repo, "deployment.status").await?;
            self.ci
                .create_deployment_status(
                    &cx,
                    &repo,
                    &r.deployment_id,
                    NewDeploymentStatus {
                        state: DeploymentState::try_from(r.state)
                            .unwrap_or(DeploymentState::Unspecified),
                        description: r.description.clone(),
                        environment: r.environment.clone(),
                        log_url: r.log_url.clone(),
                        environment_url: r.environment_url.clone(),
                        auto_inactive: r.auto_inactive,
                    },
                )
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_deployment_status(
        &self,
        req: Request<GetDeploymentStatusRequest>,
    ) -> Reply<DeploymentStatus> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            self.ci
                .get_deployment_status(&cx, &repo, &r.deployment_id, &r.status_id)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_deployment_statuses(
        &self,
        req: Request<ListDeploymentStatusesRequest>,
    ) -> Reply<ListDeploymentStatusesResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let mut all = self
                .ci
                .deployment_statuses(&cx, &repo, &r.deployment_id)
                .await?;
            all.reverse();
            let (statuses, page) = paged(all, &r.page);
            Ok(ListDeploymentStatusesResponse { statuses, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn list_pending_deployment_reviews(
        &self,
        req: Request<ListPendingDeploymentReviewsRequest>,
    ) -> Reply<ListPendingDeploymentReviewsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let admin = self.is_admin(&caller, &repo).await;
            let all = self
                .ci
                .pending_reviews(&cx, &repo, &r.environment, &caller.cx.actor_id, admin)
                .await?;
            let (reviews, page) = paged(all, &r.page);
            Ok(ListPendingDeploymentReviewsResponse { reviews, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn review_pending_deployment(
        &self,
        req: Request<ReviewPendingDeploymentRequest>,
    ) -> Reply<Deployment> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let admin = self.is_admin(&caller, &repo).await;
            let res = self
                .ci
                .review_deployment(
                    &cx,
                    &repo,
                    &r.deployment_id,
                    decision(r.decision),
                    &r.comment,
                    admin,
                )
                .await;
            if let Err(e) = &res {
                self.forge()
                    .authz
                    .audit_denial(&caller, &repo.org_id, &repo.id, "deployment.review", e)
                    .await;
            }
            res
        }
        .await;
        reply(&caller, out)
    }

    async fn list_deployment_reviews(
        &self,
        req: Request<ListDeploymentReviewsRequest>,
    ) -> Reply<ListDeploymentReviewsResponse> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            let all = self
                .ci
                .deployment_reviews(&cx, &repo, &r.deployment_id)
                .await?;
            let (reviews, page) = paged(all, &r.page);
            Ok(ListDeploymentReviewsResponse { reviews, page })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_deployment_green_status(
        &self,
        req: Request<GetDeploymentGreenStatusRequest>,
    ) -> Reply<DeploymentGreenStatus> {
        let caller = self.base.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (repo, cx) = self.read(&caller, &r.repo).await?;
            if r.r#ref.is_empty() {
                return Err(ForgeError::missing("ref"));
            }
            self.ci
                .green_status(&cx, &repo, &r.r#ref, r.changeset.clone())
                .await
        }
        .await;
        reply(&caller, out)
    }
}
