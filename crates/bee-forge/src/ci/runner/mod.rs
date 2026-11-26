//! The local runner: takes jobs through a [`RunnerApi`] and runs them on this machine.
//!
//! Each job runs in its own directory under [`RunnerConfig::work_root`] (removed afterwards):
//! `run:` steps execute through the planned shell, `uses:` steps run the built-in actions
//! (checkout through the API, `setup-*` that verify tools on `PATH`, artifacts, caches),
//! composite actions and JavaScript actions (with `node` on `PATH`), local or fetched from
//! another repository through the [`registry`] ([`RunnerConfig::actions`]). Docker actions
//! and container jobs fail with an explicit "not supported by the local runner" error.
//! Hosts: [`api::InProcessApi`] runs it inside the server process; a remote host implements
//! [`RunnerApi`] over gRPC.

pub mod actions;
pub mod api;
pub mod exec;
pub mod registry;

use std::sync::Arc;

use bee_proto::v1::WorkflowConclusion;
use tokio::sync::watch;

pub use api::{InProcessApi, RunnerApi};
pub use exec::RunnerConfig;
pub use registry::{ActionRef, ActionRegistry, ActionSource, FetchedAction, ForgeActionSource};

use crate::error::Result;

/// A finished job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// Job id.
    pub job_id: String,
    /// How it ended; `None` when the lease was lost and the server owns the outcome.
    pub conclusion: Option<WorkflowConclusion>,
}

/// A runner executing jobs locally.
pub struct LocalRunner {
    api: Arc<dyn RunnerApi>,
    config: RunnerConfig,
}

impl LocalRunner {
    /// A runner over `api`.
    pub fn new(api: Arc<dyn RunnerApi>, config: RunnerConfig) -> LocalRunner {
        LocalRunner { api, config }
    }

    /// Takes and runs one job, if one is queued.
    pub async fn run_once(&self) -> Result<Option<Ran>> {
        let Some(a) = self.api.acquire().await? else {
            return Ok(None);
        };
        let job_id = a.job_id.clone();
        let job = exec::Job::start(self.api.clone(), a, self.config.clone()).await?;
        let conclusion = job.execute().await?;
        Ok(Some(Ran { job_id, conclusion }))
    }

    /// Runs jobs until none is queued (at most `max`); returns what ran.
    pub async fn run_until_idle(&self, max: usize) -> Result<Vec<Ran>> {
        let mut out = Vec::new();
        while out.len() < max {
            match self.run_once().await? {
                Some(r) => out.push(r),
                None => break,
            }
        }
        Ok(out)
    }

    /// Runs jobs until `stop` turns true, waiting on `wake` (or `poll`) between jobs.
    pub async fn run(
        &self,
        wake: Arc<tokio::sync::Notify>,
        poll: std::time::Duration,
        mut stop: watch::Receiver<bool>,
    ) {
        loop {
            if *stop.borrow() {
                return;
            }
            let notified = wake.notified();
            if let Ok(Some(_)) = self.run_once().await {
                continue;
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(poll) => {}
                _ = stop.changed() => {}
            }
        }
    }
}
