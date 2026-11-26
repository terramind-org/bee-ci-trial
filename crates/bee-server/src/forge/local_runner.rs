//! The built-in runner enabled by `[forge.local_runner]`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use bee_forge::ci::runner::{InProcessApi, LocalRunner, RunnerConfig};
use bee_forge::ci::runners::{MANAGED_LOCAL_RUNNER_LABEL, RunnerOwner};
use bee_forge::ci::{Ci, CiServices};
use bee_forge::events::EventListener;
use bee_forge::{Cx, Forge};
use bee_proto::v1::{Event, RunnerArch, RunnerOs};
use tokio::sync::Notify;

const LABELS: &[&str] = &[
    "ubuntu-latest",
    "ubuntu-24.04",
    "ubuntu-22.04",
    "macos-latest",
    "linux",
    "self-hosted",
];

/// Warning shown whenever the in-process runner is enabled.
pub const STARTUP_WARNING: &str = "local workflow runner is on: workflow steps from every repository on this server run as this process's user on this host; set [forge.local_runner] enabled = false to turn it off";
/// Additional warning context for a runner enabled outside development mode.
pub const NON_DEV_STARTUP_WARNING: &str = "local workflow runner is on: workflow steps from every repository on this server run as this process's user on this host; set [forge.local_runner] enabled = false to turn it off. It was enabled explicitly by configuration outside --dev; jobs run as the server's OS user with access to the server's files and environment";

/// Returns the startup warning only when the in-process runner is enabled.
pub fn startup_warning(enabled: bool, dev: bool) -> Option<&'static str> {
    if !enabled {
        None
    } else if dev {
        Some(STARTUP_WARNING)
    } else {
        Some(NON_DEV_STARTUP_WARNING)
    }
}

#[derive(Default)]
/// Repository queue and notification state for the in-process workflow runner.
pub struct Pending {
    repos: Mutex<BTreeSet<String>>,
    wake: Notify,
}

/// Event listener wrapper that shares pending workflow jobs with the local runner.
pub struct QueuedJobs(pub Arc<Pending>);

#[async_trait]
impl EventListener for QueuedJobs {
    async fn on_event(&self, event: &Event) {
        if event.kind == "workflow_job.queued" && !event.repo_id.is_empty() {
            if let Ok(mut repos) = self.0.repos.lock() {
                repos.insert(event.repo_id.clone());
            }
            self.0.wake.notify_one();
        }
    }
}

struct RepoRunner {
    runner: Arc<LocalRunner>,
    busy: Arc<Mutex<bool>>,
}

/// Serves one repo-scoped runner per repo that has queued work. The forge claim transaction
/// leases a job, so replicas race safely and exactly one runner executes each job.
pub async fn run(forge: Weak<Forge>, ci: Weak<Ci>, pending: Arc<Pending>, work_root: PathBuf) {
    let mut runners = BTreeMap::<String, RepoRunner>::new();
    loop {
        let _ = tokio::time::timeout(Duration::from_secs(3), pending.wake.notified()).await;
        let (Some(forge), Some(ci)) = (forge.upgrade(), ci.upgrade()) else {
            return;
        };
        let mut repos = pending
            .repos
            .lock()
            .map(|mut repos| std::mem::take(&mut *repos))
            .unwrap_or_default();
        repos.extend(runners.keys().cloned());
        for repo_id in repos {
            if !runners.contains_key(&repo_id) {
                if let Some(runner) = register(&forge, &ci, &repo_id, &work_root).await {
                    runners.insert(repo_id.clone(), runner);
                } else {
                    continue;
                }
            }
            let Some(repo_runner) = runners.get(&repo_id) else {
                continue;
            };
            let Ok(mut busy) = repo_runner.busy.lock() else {
                continue;
            };
            if *busy {
                continue;
            }
            *busy = true;
            let runner = repo_runner.runner.clone();
            let busy = repo_runner.busy.clone();
            tokio::spawn(async move {
                let _ = runner.run_until_idle(64).await;
                if let Ok(mut busy) = busy.lock() {
                    *busy = false;
                }
            });
        }
    }
}

async fn register(
    forge: &Arc<Forge>,
    ci: &Arc<Ci>,
    repo_id: &str,
    work_root: &std::path::Path,
) -> Option<RepoRunner> {
    let cx = Cx::system();
    let repo = forge.orgs.repo_any(&cx, repo_id).await.ok()?;
    let owner = RunnerOwner::Repo(repo.id.clone(), repo.org_id.clone());
    // A prior process can leave a development runner behind. Replace only the integration's
    // marked record so an operator-managed runner with this display name survives restarts.
    for stale in ci
        .list_runners(
            &cx,
            &owner,
            "",
            "",
            bee_proto::v1::RunnerStatus::Unspecified,
            "bee-local-runner",
        )
        .await
        .ok()?
    {
        if stale
            .labels
            .iter()
            .any(|label| label.name.eq_ignore_ascii_case(MANAGED_LOCAL_RUNNER_LABEL))
        {
            ci.delete_runner(&cx, &owner, &stale.id, true).await.ok()?;
        }
    }
    let (token, _) = ci.runner_token(&cx, &owner, "register").await.ok()?;
    let mut labels: Vec<String> = LABELS.iter().map(|label| (*label).to_string()).collect();
    labels.push(MANAGED_LOCAL_RUNNER_LABEL.to_string());
    let os = if cfg!(target_os = "macos") {
        RunnerOs::Macos
    } else {
        RunnerOs::Linux
    };
    let arch = if cfg!(target_arch = "aarch64") {
        RunnerArch::Arm64
    } else {
        RunnerArch::X64
    };
    let (runner, credential) = ci
        .register_runner(
            &cx,
            &token,
            "bee-local-runner",
            os,
            arch,
            &labels,
            "",
            false,
            env!("CARGO_PKG_VERSION"),
        )
        .await
        .ok()?;
    let api = InProcessApi::new(
        CiServices::new(forge.clone(), ci.clone()),
        runner.id,
        credential,
        Vec::new(),
    );
    let config = RunnerConfig {
        work_root: work_root.join(repo.id),
        name: "bee-local-runner".into(),
        heartbeat: Duration::from_secs(5),
        ..RunnerConfig::default()
    };
    Some(RepoRunner {
        runner: Arc::new(LocalRunner::new(Arc::new(api), config)),
        busy: Arc::new(Mutex::new(false)),
    })
}

#[cfg(test)]
mod tests {
    use super::{NON_DEV_STARTUP_WARNING, STARTUP_WARNING, startup_warning};

    #[test]
    fn startup_warning_covers_dev_and_explicit_non_dev_enablement() {
        assert_eq!(startup_warning(true, true), Some(STARTUP_WARNING));
        assert_eq!(startup_warning(true, false), Some(NON_DEV_STARTUP_WARNING));
        assert_eq!(startup_warning(false, false), None);
        assert_eq!(startup_warning(false, true), None);
    }
}
