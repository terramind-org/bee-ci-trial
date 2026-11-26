//! How a bisect tests a candidate: the [`BisectProber`] port and [`CiProber`], which re-runs
//! the failing workflow job at the candidate through the CI engine.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{ObjectId, Repo};

use super::plan::Outcome;
use crate::ci::Ci;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};

/// What a probe re-runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeTarget {
    /// The protected ref being bisected (bee name, `main`).
    pub git_ref: String,
    /// The failing check's name.
    pub check_name: String,
    /// Workflow file of the failing job (empty for checks reported by external CI).
    pub workflow_path: String,
    /// Job key in that workflow.
    pub job_key: String,
}

/// Tests candidates for a bisect.
#[async_trait]
pub trait BisectProber: Send + Sync {
    /// Starts testing `changeset`; returns a handle for [`BisectProber::poll`]. An error makes
    /// the candidate untestable (skipped).
    async fn start(
        &self,
        cx: &Cx,
        repo: &Repo,
        target: &ProbeTarget,
        changeset: &ObjectId,
    ) -> Result<String>;
    /// The outcome of a started probe; `None` while it runs.
    async fn poll(
        &self,
        cx: &Cx,
        repo: &Repo,
        target: &ProbeTarget,
        probe_id: &str,
    ) -> Result<Option<Outcome>>;
    /// Stops a probe that is no longer needed (best effort).
    async fn cancel(&self, cx: &Cx, repo: &Repo, probe_id: &str) -> Result<()>;
}

/// [`BisectProber`] over the CI engine: each probe is a probe run of the failing job and
/// what it needs ([`Ci::start_probe_run`]); the job's conclusion is the outcome.
pub struct CiProber {
    ci: Arc<Ci>,
}

impl CiProber {
    /// A prober over `ci`.
    pub fn new(ci: Arc<Ci>) -> CiProber {
        CiProber { ci }
    }
}

#[async_trait]
impl BisectProber for CiProber {
    async fn start(
        &self,
        cx: &Cx,
        repo: &Repo,
        target: &ProbeTarget,
        changeset: &ObjectId,
    ) -> Result<String> {
        if target.workflow_path.is_empty() || target.job_key.is_empty() {
            return Err(ForgeError::invalid(
                "not_a_workflow_check",
                format!(
                    "{} is not reported by a workflow job; CI cannot re-run it",
                    target.check_name
                ),
            ));
        }
        self.ci
            .start_probe_run(
                cx,
                repo,
                &target.workflow_path,
                &target.job_key,
                changeset,
                &target.git_ref,
            )
            .await
    }

    async fn poll(
        &self,
        cx: &Cx,
        repo: &Repo,
        target: &ProbeTarget,
        probe_id: &str,
    ) -> Result<Option<Outcome>> {
        Ok(self
            .ci
            .probe_outcome(cx, &repo.id, probe_id, &target.job_key, &target.check_name)
            .await?
            .map(|o| match o {
                Some(true) => Outcome::Pass,
                Some(false) => Outcome::Fail,
                None => Outcome::Skip,
            }))
    }

    async fn cancel(&self, cx: &Cx, repo: &Repo, probe_id: &str) -> Result<()> {
        self.ci.cancel_run(cx, &repo.id, probe_id, false).await
    }
}
