//! What the local runner needs from Beehive ([`RunnerApi`]) and the in-process
//! implementation over [`CiServices`] ([`InProcessApi`]).
//!
//! Job-protocol calls carry the runner credential, artifact and cache calls the job token,
//! exactly as a remote runner would send them; transfers to signed URLs and the one-time
//! secrets URL go to the handlers those URLs name. A gRPC client implementation lives with
//! the binary that hosts a remote runner (`bee-cli`/`bee-server`); this crate cannot depend
//! on them.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bee_proto::v1::runner_service_server::RunnerService;
use bee_proto::v1::workflow_service_server::WorkflowService;
use bee_proto::v1::*;

use crate::ci::service::CiServices;
use crate::ci::vcs::TreeFile;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};

/// The Beehive calls a runner makes.
#[async_trait]
pub trait RunnerApi: Send + Sync {
    /// Takes the next job, if one is queued for this runner.
    async fn acquire(&self) -> Result<Option<RunnerJobAssignment>>;
    /// Renews a lease; reports cancellation.
    async fn renew(&self, job_id: &str, lease_id: &str) -> Result<RenewRunnerJobResponse>;
    /// Reports a step change.
    async fn update_step(&self, req: UpdateRunnerJobStepRequest) -> Result<()>;
    /// Appends log bytes at `offset`; returns the stored end.
    async fn append_log(
        &self,
        job_id: &str,
        lease_id: &str,
        offset: u64,
        data: Vec<u8>,
        step: u32,
    ) -> Result<u64>;
    /// Completes the job.
    async fn complete(&self, req: CompleteRunnerJobRequest) -> Result<()>;
    /// Redeems the job's secrets URL once.
    async fn secrets(&self, a: &RunnerJobAssignment) -> Result<BTreeMap<String, String>>;
    /// Every file of the job's repo at `at` (the job's head when `None`).
    async fn checkout(
        &self,
        a: &RunnerJobAssignment,
        at: Option<ObjectId>,
    ) -> Result<Vec<TreeFile>>;
    /// Reserves an artifact; returns its id and upload URL.
    async fn create_artifact(
        &self,
        a: &RunnerJobAssignment,
        name: &str,
        retention_days: u32,
    ) -> Result<(String, String)>;
    /// Uploads bytes to a signed upload URL.
    async fn upload(&self, a: &RunnerJobAssignment, url: &str, bytes: Vec<u8>) -> Result<()>;
    /// Seals an uploaded artifact.
    async fn finalize_artifact(
        &self,
        a: &RunnerJobAssignment,
        id: &str,
        size: u64,
        digest: &str,
    ) -> Result<()>;
    /// Artifacts of the job's run.
    async fn list_artifacts(&self, a: &RunnerJobAssignment) -> Result<Vec<WorkflowArtifact>>;
    /// Downloads an artifact's bytes.
    async fn download_artifact(&self, a: &RunnerJobAssignment, id: &str) -> Result<Vec<u8>>;
    /// Looks up a cache entry; returns it, whether the key matched exactly, and its bytes.
    async fn cache_lookup(
        &self,
        a: &RunnerJobAssignment,
        key: &str,
        version: &str,
        restore_keys: &[String],
    ) -> Result<Option<(WorkflowCache, bool, Vec<u8>)>>;
    /// Saves a cache entry (reserve, upload, commit).
    async fn cache_save(
        &self,
        a: &RunnerJobAssignment,
        key: &str,
        version: &str,
        bytes: Vec<u8>,
    ) -> Result<()>;
}

/// [`RunnerApi`] calling [`CiServices`] in process.
pub struct InProcessApi {
    services: CiServices,
    runner_id: String,
    credential: String,
    labels: Vec<String>,
}

fn req<T>(token: &str, msg: T) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    if let Ok(v) = format!("Bearer {token}").parse() {
        r.metadata_mut().insert("authorization", v);
    }
    r
}

fn status_err(s: tonic::Status) -> ForgeError {
    match bee_proto::errors::detail_from_status(&s) {
        Some(d) => {
            let reason =
                bee_proto::errors::parse_reason(&d.reason).unwrap_or(ErrorReason::Unavailable);
            let mut e = ForgeError::new(reason, d.message);
            if !d.subcode.is_empty() {
                e = e.with_subcode(&d.subcode);
            }
            e
        }
        None => ForgeError::unavailable(s.message().to_string()),
    }
}

impl InProcessApi {
    /// An API for runner `runner_id` holding `credential`, offering `labels` (empty = its
    /// registered labels).
    pub fn new(
        services: CiServices,
        runner_id: impl Into<String>,
        credential: impl Into<String>,
        labels: Vec<String>,
    ) -> InProcessApi {
        InProcessApi {
            services,
            runner_id: runner_id.into(),
            credential: credential.into(),
            labels,
        }
    }

    fn cx(&self) -> Cx {
        Cx::system()
    }

    /// Checks a signed URL and returns its path segments.
    fn signed(&self, url: &str) -> Result<Vec<String>> {
        let path = self
            .services
            .ci()
            .url_signer()
            .verify(url, self.services.ci().now())?;
        Ok(path
            .trim_start_matches('/')
            .split('/')
            .map(str::to_string)
            .collect())
    }

    async fn job_repo(&self, a: &RunnerJobAssignment) -> Result<Repo> {
        let caller = self
            .services
            .base
            .caller(&req(&a.job_token, ()))
            .map_err(status_err)?;
        let (repo, _) = self.services.read(&caller, &a.repo).await?;
        if caller.cx.actor_id != format!("job:{}", a.job_id) {
            return Err(ForgeError::permission_denied("not this job's token"));
        }
        Ok(repo)
    }
}

#[async_trait]
impl RunnerApi for InProcessApi {
    async fn acquire(&self) -> Result<Option<RunnerJobAssignment>> {
        let caller = self
            .services
            .base
            .caller(&req(&self.credential, ()))
            .map_err(status_err)?;
        let ids = vec![
            caller.token.token_id.0.clone(),
            caller.token.root_token_id.0.clone(),
        ];
        let ci = self.services.ci();
        ci.authenticate_runner(&caller.cx, &caller.cx.actor_id, &ids, &self.runner_id)
            .await?;
        ci.claim(&caller.cx, &self.runner_id, &self.labels, 1).await
    }

    async fn renew(&self, job_id: &str, lease_id: &str) -> Result<RenewRunnerJobResponse> {
        self.services
            .renew_runner_job(req(
                &self.credential,
                RenewRunnerJobRequest {
                    runner_id: self.runner_id.clone(),
                    job_id: job_id.into(),
                    lease_id: lease_id.into(),
                },
            ))
            .await
            .map(|r| r.into_inner())
            .map_err(status_err)
    }

    async fn update_step(&self, mut r: UpdateRunnerJobStepRequest) -> Result<()> {
        r.runner_id = self.runner_id.clone();
        self.services
            .update_runner_job_step(req(&self.credential, r))
            .await
            .map(|_| ())
            .map_err(status_err)
    }

    async fn append_log(
        &self,
        job_id: &str,
        lease_id: &str,
        offset: u64,
        data: Vec<u8>,
        step: u32,
    ) -> Result<u64> {
        self.services
            .append_runner_job_log(req(
                &self.credential,
                AppendRunnerJobLogRequest {
                    runner_id: self.runner_id.clone(),
                    job_id: job_id.into(),
                    lease_id: lease_id.into(),
                    offset,
                    data,
                    step_number: step,
                },
            ))
            .await
            .map(|r| r.into_inner().stored_end)
            .map_err(status_err)
    }

    async fn complete(&self, mut r: CompleteRunnerJobRequest) -> Result<()> {
        r.runner_id = self.runner_id.clone();
        self.services
            .complete_runner_job(req(&self.credential, r))
            .await
            .map(|_| ())
            .map_err(status_err)
    }

    async fn secrets(&self, a: &RunnerJobAssignment) -> Result<BTreeMap<String, String>> {
        let seg = self.signed(&a.secrets_url)?;
        match seg.as_slice() {
            [p, jobs, job, s, nonce]
                if p == "_ci" && jobs == "jobs" && s == "secrets" && *job == a.job_id =>
            {
                self.services
                    .ci()
                    .redeem_secrets(&self.cx(), job, nonce)
                    .await
            }
            _ => Err(ForgeError::permission_denied("not a secrets URL")),
        }
    }

    async fn checkout(
        &self,
        a: &RunnerJobAssignment,
        at: Option<ObjectId>,
    ) -> Result<Vec<TreeFile>> {
        let repo = self.job_repo(a).await?;
        let at = at
            .or_else(|| a.head_changeset.clone())
            .ok_or_else(|| ForgeError::missing("head_changeset"))?;
        self.services.ci().vcs.checkout(&repo, &at).await
    }

    async fn create_artifact(
        &self,
        a: &RunnerJobAssignment,
        name: &str,
        retention_days: u32,
    ) -> Result<(String, String)> {
        let r = self
            .services
            .create_workflow_artifact_upload(req(
                &a.job_token,
                CreateWorkflowArtifactUploadRequest {
                    repo: a.repo.clone(),
                    job_id: a.job_id.clone(),
                    name: name.into(),
                    archive: true,
                    content_type: "application/zip".into(),
                    retention_days,
                },
            ))
            .await
            .map_err(status_err)?
            .into_inner();
        Ok((r.artifact_id, r.upload_url))
    }

    async fn upload(&self, _a: &RunnerJobAssignment, url: &str, bytes: Vec<u8>) -> Result<()> {
        let seg = self.signed(url)?;
        let ci = self.services.ci();
        match seg.as_slice() {
            [p, repos, repo, kind, id, op] if p == "_ci" && repos == "repos" && op == "upload" => {
                match kind.as_str() {
                    "artifacts" => ci.upload_artifact(&self.cx(), repo, id, bytes).await,
                    "caches" => ci.upload_cache(&self.cx(), repo, id, bytes).await,
                    _ => Err(ForgeError::permission_denied("not an upload URL")),
                }
            }
            _ => Err(ForgeError::permission_denied("not an upload URL")),
        }
    }

    async fn finalize_artifact(
        &self,
        a: &RunnerJobAssignment,
        id: &str,
        size: u64,
        digest: &str,
    ) -> Result<()> {
        self.services
            .finalize_workflow_artifact(req(
                &a.job_token,
                FinalizeWorkflowArtifactRequest {
                    repo: a.repo.clone(),
                    artifact_id: id.into(),
                    size_bytes: size,
                    digest: digest.into(),
                },
            ))
            .await
            .map(|_| ())
            .map_err(status_err)
    }

    async fn list_artifacts(&self, a: &RunnerJobAssignment) -> Result<Vec<WorkflowArtifact>> {
        self.services
            .list_workflow_artifacts(req(
                &a.job_token,
                ListWorkflowArtifactsRequest {
                    repo: a.repo.clone(),
                    run_id: a.run_id.clone(),
                    name: String::new(),
                    page: Some(PageRequest {
                        page_size: 500,
                        page_token: String::new(),
                    }),
                },
            ))
            .await
            .map(|r| r.into_inner().artifacts)
            .map_err(status_err)
    }

    async fn download_artifact(&self, a: &RunnerJobAssignment, id: &str) -> Result<Vec<u8>> {
        let url = self
            .services
            .get_workflow_artifact_download_url(req(
                &a.job_token,
                GetWorkflowArtifactDownloadUrlRequest {
                    repo: a.repo.clone(),
                    artifact_id: id.into(),
                },
            ))
            .await
            .map_err(status_err)?
            .into_inner()
            .url;
        let seg = self.signed(&url)?;
        match seg.as_slice() {
            [p, repos, repo, kind, id, op]
                if p == "_ci" && repos == "repos" && kind == "artifacts" && op == "download" =>
            {
                self.services
                    .ci()
                    .artifact_bytes(&self.cx(), repo, id)
                    .await
            }
            _ => Err(ForgeError::permission_denied("not a download URL")),
        }
    }

    async fn cache_lookup(
        &self,
        a: &RunnerJobAssignment,
        key: &str,
        version: &str,
        restore_keys: &[String],
    ) -> Result<Option<(WorkflowCache, bool, Vec<u8>)>> {
        let res = self
            .services
            .lookup_workflow_cache(req(
                &a.job_token,
                LookupWorkflowCacheRequest {
                    repo: a.repo.clone(),
                    job_id: a.job_id.clone(),
                    key: key.into(),
                    version: version.into(),
                    restore_keys: restore_keys.to_vec(),
                },
            ))
            .await;
        let r = match res {
            Ok(r) => r.into_inner(),
            Err(s) => {
                let e = status_err(s);
                if e.reason == ErrorReason::NotFound {
                    return Ok(None);
                }
                return Err(e);
            }
        };
        let seg = self.signed(&r.download_url)?;
        let bytes = match seg.as_slice() {
            [p, repos, repo, kind, id, op]
                if p == "_ci" && repos == "repos" && kind == "caches" && op == "download" =>
            {
                self.services.ci().cache_bytes(&self.cx(), repo, id).await?
            }
            _ => return Err(ForgeError::permission_denied("not a download URL")),
        };
        Ok(r.cache.map(|c| (c, r.exact_match, bytes)))
    }

    async fn cache_save(
        &self,
        a: &RunnerJobAssignment,
        key: &str,
        version: &str,
        bytes: Vec<u8>,
    ) -> Result<()> {
        let size = bytes.len() as u64;
        let digest = crate::ci::util::sha256_digest(&bytes);
        let r = self
            .services
            .reserve_workflow_cache(req(
                &a.job_token,
                ReserveWorkflowCacheRequest {
                    repo: a.repo.clone(),
                    job_id: a.job_id.clone(),
                    key: key.into(),
                    version: version.into(),
                    size_bytes: size,
                },
            ))
            .await
            .map_err(status_err)?
            .into_inner();
        self.upload(a, &r.upload_url, bytes).await?;
        self.services
            .commit_workflow_cache(req(
                &a.job_token,
                CommitWorkflowCacheRequest {
                    repo: a.repo.clone(),
                    cache_id: r.cache_id,
                    size_bytes: size,
                    digest,
                },
            ))
            .await
            .map(|_| ())
            .map_err(status_err)
    }
}
