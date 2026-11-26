//! Refresh attributable work when agent work or its pull request changes state.

use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{Event, SafetyRef, SafetyWorkKind};

use super::{evidence, service::SafetyApi};
use crate::agents::{Agents, registry::repo_ref_id};
use crate::cx::Cx;
use crate::error::Result;
use crate::events::EventListener;

pub(crate) struct SafetyListener {
    agents: Arc<Agents>,
}

impl SafetyListener {
    pub(crate) fn new(agents: Arc<Agents>) -> Self {
        Self { agents }
    }

    async fn refresh(&self, event: &Event) -> Result<()> {
        let Some(subject) = event.subject.as_ref() else {
            return Ok(());
        };
        let cx = Cx::system();
        let repo = self.agents.forge.orgs.repo_any(&cx, &event.repo_id).await?;
        let work = match event.kind.as_str() {
            "swarm.state_changed" | "swarm.task_finished" => SafetyRef {
                repo: repo_ref_id(&repo.id),
                kind: SafetyWorkKind::Swarm as i32,
                id: subject.id.clone(),
            },
            "agent_run.completed" | "safety.evidence_observed" => {
                evidence::work_for_run(&self.agents, &cx, &repo, &subject.id).await?
            }
            _ => return Ok(()),
        };
        SafetyApi::new(self.agents.clone())
            .refresh(&repo, &cx, work)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl EventListener for SafetyListener {
    async fn on_event(&self, event: &Event) {
        if !matches!(
            event.kind.as_str(),
            "swarm.state_changed"
                | "swarm.task_finished"
                | "agent_run.completed"
                | "safety.evidence_observed"
        ) {
            return;
        }
        if let Err(error) = self.refresh(event).await {
            eprintln!(
                "Safety evidence refresh failed for {} in {}: {error}",
                event.kind, event.repo_id
            );
        }
    }
}
