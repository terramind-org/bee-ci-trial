//! `ModelService`.
//!
//! | RPC | access |
//! |---|---|
//! | `ListModels`, `GetModel` | any caller; with `org`, an org member (`read` token) |
//! | `RunModelInference`, `StreamModelInference` | org member; admitted, then served elsewhere |
//! | `ListPromptFiles`, `GetPromptFile`, `GetEvalRun`, `ListEvalRuns` | `read` on the repo |
//! | `RunPromptEval`, `CancelEvalRun` | `commit` (write role) on the repo |
//! | provider keys | org owner or admin |
//! | `GetModelUsage` | the caller's own usage as a member; anyone's as owner or admin |

use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::model_service_server::{ModelService, ModelServiceServer};
use bee_proto::v1::{
    CancelEvalRunRequest, CreateModelProviderKeyRequest, DeleteModelProviderKeyRequest,
    DeleteModelProviderKeyResponse, EvalRun, EvalRunState, GetEvalRunRequest, GetModelRequest,
    GetModelUsageRequest, GetPromptFileRequest, ListEvalRunsRequest, ListEvalRunsResponse,
    ListModelProviderKeysRequest, ListModelProviderKeysResponse, ListModelsRequest,
    ListModelsResponse, ListPromptFilesRequest, ListPromptFilesResponse, ModelInferenceChunk,
    ModelInfo, ModelMessage, ModelParams, ModelProviderKey, ModelTool, ModelUsage, PromptFile,
    RunModelInferenceRequest, RunModelInferenceResponse, RunPromptEvalRequest,
    StreamModelInferenceRequest,
};
use tonic::{Request, Response, Status};

use super::{EvalFilter, InferenceEstimate, Models};
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::enterprise::common::page_args;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::service::{ForgeServices, ResponseStream, next, reply, unserved};

const GATEWAY: &str = "the model gateway process";

/// What admission needs of an inference request.
struct InferenceReq<'a> {
    org: &'a str,
    model: &'a str,
    key: &'a str,
    messages: &'a [ModelMessage],
    tools: &'a [ModelTool],
    params: &'a Option<ModelParams>,
}

/// `ModelService` over [`Models`].
#[derive(Clone)]
pub struct ModelsService {
    models: Arc<Models>,
    services: ForgeServices,
}

impl ModelsService {
    /// The service.
    pub fn new(models: Arc<Models>, services: ForgeServices) -> ModelsService {
        ModelsService { models, services }
    }

    /// The tonic server to register.
    pub fn server(
        models: Arc<Models>,
        services: ForgeServices,
    ) -> ModelServiceServer<ModelsService> {
        ModelServiceServer::new(ModelsService::new(models, services))
    }

    /// Resolves `org` and authorizes the caller as a member; an empty `org` is no org.
    async fn member_org(&self, caller: &Caller, org: &str) -> Result<(String, Cx)> {
        if org.is_empty() {
            return Ok((String::new(), caller.cx.clone()));
        }
        let id = self.services.org_id(caller, org).await?;
        let cx = self
            .services
            .forge()
            .authz
            .org(caller, &id, OrgPerm::ViewMembers)
            .await?;
        Ok((id, cx))
    }

    /// Resolves `org` and authorizes the caller as owner or admin; denials are audited.
    async fn admin_org(&self, caller: &Caller, org: &str, action: &str) -> Result<(String, Cx)> {
        if org.is_empty() {
            return Err(ForgeError::missing("org"));
        }
        let id = self.services.org_id(caller, org).await?;
        let auth = self
            .services
            .forge()
            .authz
            .org(caller, &id, OrgPerm::Manage)
            .await;
        let cx = self.services.guarded(caller, &id, "", action, auth).await?;
        Ok((id, cx))
    }

    /// Admits an inference request (org policy, routing, budgets); the caller then answers
    /// `UNIMPLEMENTED` / `served_elsewhere`, because the gateway process runs the model.
    async fn admit(&self, caller: &Caller, q: InferenceReq<'_>) -> Result<()> {
        let (org_id, cx) = self.member_org(caller, q.org).await?;
        let bytes: usize = q.messages.iter().map(|m| m.content.len()).sum::<usize>()
            + q.tools
                .iter()
                .map(|t| t.name.len() + t.description.len() + t.parameters_schema.len())
                .sum::<usize>();
        let est = InferenceEstimate {
            input_tokens: bytes.div_ceil(4) as u64,
            max_output_tokens: q.params.as_ref().map(|p| p.max_output_tokens).unwrap_or(0),
        };
        self.models
            .admit_inference(&cx, &org_id, q.model, q.key, est)
            .await?;
        Ok(())
    }
}

#[tonic::async_trait]
impl ModelService for ModelsService {
    async fn list_models(
        &self,
        req: Request<ListModelsRequest>,
    ) -> std::result::Result<Response<ListModelsResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.member_org(&caller, &r.org).await?;
            let (token, size) = page_args(&r.page);
            let (models, token) = self
                .models
                .list_models(&cx, &org_id, &r.provider, &r.capability, &token, size)
                .await?;
            Ok(ListModelsResponse {
                models,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_model(
        &self,
        req: Request<GetModelRequest>,
    ) -> std::result::Result<Response<ModelInfo>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.member_org(&caller, &r.org).await?;
            self.models.get_model(&cx, &org_id, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn run_model_inference(
        &self,
        req: Request<RunModelInferenceRequest>,
    ) -> std::result::Result<Response<RunModelInferenceResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let q = InferenceReq {
            org: &r.org,
            model: &r.model,
            key: &r.provider_key_id,
            messages: &r.messages,
            tools: &r.tools,
            params: &r.params,
        };
        let out: Result<RunModelInferenceResponse> = match self.admit(&caller, q).await {
            Ok(()) => Err(unserved("RunModelInference", GATEWAY)),
            Err(e) => Err(e),
        };
        reply(&caller, out)
    }

    type StreamModelInferenceStream = ResponseStream<ModelInferenceChunk>;

    async fn stream_model_inference(
        &self,
        req: Request<StreamModelInferenceRequest>,
    ) -> std::result::Result<Response<Self::StreamModelInferenceStream>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let q = InferenceReq {
            org: &r.org,
            model: &r.model,
            key: &r.provider_key_id,
            messages: &r.messages,
            tools: &r.tools,
            params: &r.params,
        };
        let out: Result<Self::StreamModelInferenceStream> = match self.admit(&caller, q).await {
            Ok(()) => Err(unserved("StreamModelInference", GATEWAY)),
            Err(e) => Err(e),
        };
        reply(&caller, out)
    }

    async fn list_prompt_files(
        &self,
        req: Request<ListPromptFilesRequest>,
    ) -> std::result::Result<Response<ListPromptFilesResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.services.repo(&caller, &r.repo).await?;
            let cx = self
                .services
                .forge()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let (token, size) = page_args(&r.page);
            let (files, token) = self
                .models
                .list_prompt_files(&cx, &repo.id, &r.path_prefix, &token, size)
                .await?;
            Ok(ListPromptFilesResponse {
                prompt_files: files,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn get_prompt_file(
        &self,
        req: Request<GetPromptFileRequest>,
    ) -> std::result::Result<Response<PromptFile>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.services.repo(&caller, &r.repo).await?;
            let cx = self
                .services
                .forge()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            self.models
                .get_prompt_file(&cx, &repo.id, &r.path, r.include_raw)
                .await
        }
        .await;
        reply(&caller, out)
    }

    async fn run_prompt_eval(
        &self,
        req: Request<RunPromptEvalRequest>,
    ) -> std::result::Result<Response<EvalRun>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.services.repo(&caller, &r.repo).await?;
            let auth = self
                .services
                .forge()
                .authz
                .repo(&caller, &repo, Action::Commit, &[], None, None)
                .await;
            let cx = self
                .services
                .guarded(&caller, &repo.org_id, &repo.id, "model.eval.run", auth)
                .await?;
            if repo.archived {
                return Err(ForgeError::archived(&repo.full_name));
            }
            let changeset = match r.revision.as_ref().map(|v| v.expr.as_str()) {
                Some(expr) if !expr.is_empty() => Some(
                    self.services
                        .forge()
                        .hooks
                        .resolver
                        .resolve(&repo, expr)
                        .await?,
                ),
                _ => None,
            };
            self.models.run_prompt_eval(&cx, &repo, changeset, &r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn get_eval_run(
        &self,
        req: Request<GetEvalRunRequest>,
    ) -> std::result::Result<Response<EvalRun>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.services.repo(&caller, &r.repo).await?;
            let cx = self
                .services
                .forge()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            self.models.get_eval_run(&cx, &repo.id, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_eval_runs(
        &self,
        req: Request<ListEvalRunsRequest>,
    ) -> std::result::Result<Response<ListEvalRunsResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.services.repo(&caller, &r.repo).await?;
            let cx = self
                .services
                .forge()
                .authz
                .repo(&caller, &repo, Action::Read, &[], None, None)
                .await?;
            let (token, size) = page_args(&r.page);
            let (runs, token) = self
                .models
                .list_eval_runs(
                    &cx,
                    &repo.id,
                    &EvalFilter {
                        prompt_path: r.prompt_path,
                        changeset: r.changeset,
                        state: EvalRunState::try_from(r.state).unwrap_or_default(),
                    },
                    &token,
                    size,
                )
                .await?;
            Ok(ListEvalRunsResponse {
                runs,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn cancel_eval_run(
        &self,
        req: Request<CancelEvalRunRequest>,
    ) -> std::result::Result<Response<EvalRun>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let repo = self.services.repo(&caller, &r.repo).await?;
            let auth = self
                .services
                .forge()
                .authz
                .repo(&caller, &repo, Action::Commit, &[], None, None)
                .await;
            let cx = self
                .services
                .guarded(&caller, &repo.org_id, &repo.id, "model.eval.cancel", auth)
                .await?;
            self.models.cancel_eval_run(&cx, &repo.id, &r.id).await
        }
        .await;
        reply(&caller, out)
    }

    async fn create_model_provider_key(
        &self,
        req: Request<CreateModelProviderKeyRequest>,
    ) -> std::result::Result<Response<ModelProviderKey>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.admin_org(&caller, &r.org, "model.key.create").await?;
            self.models.create_provider_key(&cx, &org_id, r).await
        }
        .await;
        reply(&caller, out)
    }

    async fn list_model_provider_keys(
        &self,
        req: Request<ListModelProviderKeysRequest>,
    ) -> std::result::Result<Response<ListModelProviderKeysResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if r.org.is_empty() {
                return Err(ForgeError::missing("org"));
            }
            let org_id = self.services.org_id(&caller, &r.org).await?;
            let cx = self
                .services
                .forge()
                .authz
                .org(&caller, &org_id, OrgPerm::Manage)
                .await?;
            let (token, size) = page_args(&r.page);
            let (keys, token) = self
                .models
                .list_provider_keys(&cx, &org_id, &token, size)
                .await?;
            Ok(ListModelProviderKeysResponse {
                keys,
                page: next(token),
            })
        }
        .await;
        reply(&caller, out)
    }

    async fn delete_model_provider_key(
        &self,
        req: Request<DeleteModelProviderKeyRequest>,
    ) -> std::result::Result<Response<DeleteModelProviderKeyResponse>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            let (org_id, cx) = self.admin_org(&caller, &r.org, "model.key.delete").await?;
            self.models.delete_provider_key(&cx, &org_id, &r.id).await?;
            Ok(DeleteModelProviderKeyResponse {})
        }
        .await;
        reply(&caller, out)
    }

    async fn get_model_usage(
        &self,
        req: Request<GetModelUsageRequest>,
    ) -> std::result::Result<Response<ModelUsage>, Status> {
        let caller = self.services.caller(&req)?;
        let r = req.into_inner();
        let out = async {
            if r.org.is_empty() {
                return Err(ForgeError::missing("org"));
            }
            let org_id = self.services.org_id(&caller, &r.org).await?;
            let own = !r.principal_id.is_empty() && r.principal_id == caller.cx.actor_id;
            let perm = if own {
                OrgPerm::ViewMembers
            } else {
                OrgPerm::Manage
            };
            let cx = self
                .services
                .forge()
                .authz
                .org(&caller, &org_id, perm)
                .await?;
            self.models
                .usage(
                    &cx,
                    &org_id,
                    &r.principal_id,
                    r.window_start.as_ref().map(ts_ms),
                    r.window_end.as_ref().map(ts_ms),
                    r.group_by_principal,
                )
                .await
        }
        .await;
        reply(&caller, out)
    }
}
