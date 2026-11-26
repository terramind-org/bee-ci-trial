//! Retry safety refreshes from the outbox item that caused them.

use std::sync::{Arc, Weak};

use bee_proto::v1::{AuditEntry, ErrorReason, Event, Repo, SafetyRef, SafetyWorkKind, repo_ref};

use super::{detectors, evidence, service::SafetyApi};
use crate::agents::{Agents, registry::repo_ref_id};
use crate::cx::Cx;
use crate::error::Result;

pub(crate) struct SafetyListener {
    agents: Weak<Agents>,
}

impl SafetyListener {
    pub(crate) fn new(agents: &Arc<Agents>) -> Self {
        Self {
            agents: Arc::downgrade(agents),
        }
    }

    pub(crate) async fn observe(
        &self,
        event: Option<&Event>,
        audit: Option<&AuditEntry>,
    ) -> Result<()> {
        let Some(agents) = self.agents.upgrade() else {
            return Ok(());
        };
        let result = if let Some(event) = event {
            Self::event(&agents, event).await
        } else if let Some(audit) = audit {
            Self::audit(&agents, audit).await
        } else {
            Ok(())
        };
        if let Err(error) = &result {
            if let Some(event) = event {
                eprintln!(
                    "Safety refresh for {} in {} failed; will retry: {error}",
                    event.kind, event.repo_id
                );
            } else if let Some(audit) = audit {
                eprintln!(
                    "Safety refresh for audit {} failed; will retry: {error}",
                    audit.id
                );
            }
        }
        result
    }

    async fn event(agents: &Arc<Agents>, event: &Event) -> Result<()> {
        let checks = matches!(
            event.kind.as_str(),
            "check.updated"
                | "check_run.rerequested"
                | "check_suite.rerequested"
                | "workflow_run.completed"
        );
        let all = checks
            || matches!(
                event.kind.as_str(),
                "code_scanning.analysis_processed"
                    | "code_scanning_alert.fixed"
                    | "code_scanning_alert.closed_by_user"
                    | "code_scanning_alert.reopened"
            );
        if !all
            && !matches!(
                event.kind.as_str(),
                "swarm.state_changed"
                    | "swarm.task_finished"
                    | "agent_run.completed"
                    | "safety.evidence_observed"
            )
        {
            return Ok(());
        }
        let cx = Cx::system();
        let Some(repo) = repo(agents, &cx, &event.repo_id).await? else {
            return Ok(());
        };
        if all {
            return Self::repository(agents, &repo, checks).await;
        }
        let Some(subject) = event.subject.as_ref() else {
            return Ok(());
        };
        let work = match event.kind.as_str() {
            "swarm.state_changed" | "swarm.task_finished" => SafetyRef {
                repo: repo_ref_id(&repo.id),
                kind: SafetyWorkKind::Swarm as i32,
                id: subject.id.clone(),
            },
            _ => evidence::work_for_run(agents, &cx, &repo, &subject.id).await?,
        };
        Self::refresh(agents, &repo, work).await
    }

    async fn audit(agents: &Arc<Agents>, entry: &AuditEntry) -> Result<()> {
        if entry.result != "ok" || !detectors::leash_audit_action(&entry.action) {
            return Ok(());
        }
        let Some(credential) = detectors::audit_credential(entry) else {
            return Ok(());
        };
        let Some(run_id) = credential["run"].as_str() else {
            return Ok(());
        };
        let cx = Cx::system();
        let run = match agents.run_rec(&cx, run_id).await {
            Ok(run) => run,
            Err(error) if error.reason == ErrorReason::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !detectors::leash_action(entry, &run.token_id) {
            return Ok(());
        }
        let Some(repo_ref::Repo::Id(id)) = run.run().repo.and_then(|reference| reference.repo)
        else {
            return Ok(());
        };
        let Some(repo) = repo(agents, &cx, &id).await? else {
            return Ok(());
        };
        let work = evidence::work_for_run(agents, &cx, &repo, run_id).await?;
        Self::refresh(agents, &repo, work).await
    }

    async fn repository(agents: &Arc<Agents>, repo: &Repo, checks: bool) -> Result<()> {
        let mut after = String::new();
        let mut failure = None;
        loop {
            let (works, next) =
                evidence::list_refs(agents, &Cx::system(), repo, &after, 40).await?;
            for work in works {
                if checks && work.kind() != SafetyWorkKind::Swarm {
                    continue;
                }
                if let Err(error) = Self::refresh(agents, repo, work).await {
                    failure.get_or_insert(error);
                }
            }
            if next.is_empty() {
                return failure.map_or(Ok(()), Err);
            }
            after = next;
        }
    }

    async fn refresh(agents: &Arc<Agents>, repo: &Repo, work: SafetyRef) -> Result<()> {
        match SafetyApi::new(agents.clone())
            .refresh(repo, &Cx::system(), work)
            .await
        {
            Ok(_) => Ok(()),
            Err(error)
                if matches!(
                    error.subcode.as_str(),
                    "safety_work_missing" | "swarm_missing"
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

async fn repo(agents: &Agents, cx: &Cx, id: &str) -> Result<Option<Repo>> {
    match agents.forge.orgs.repo_any(cx, id).await {
        Ok(repo) => Ok(Some(repo)),
        Err(error) if error.reason == ErrorReason::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
