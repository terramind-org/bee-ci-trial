//! Authorization, work materialization, and mutations for SafetyService.

use std::sync::Arc;

use bee_policy::Action;
use bee_proto::v1::safety_service_server::{SafetyService, SafetyServiceServer};
use bee_proto::v1::*;
use serde_json::json;
use tonic::{Request, Response, Status};

use super::{control, detectors, evidence, store};
use crate::agents::registry::repo_ref_id;
use crate::agents::{Agents, SURFACE};
use crate::auth::{Caller, OrgPerm};
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::{ts, ts_ms};
use crate::service::{ForgeServices, next, page, reply};
use crate::store::Tx;

mod attention;
mod oversight;
mod review;

type Rpc<T> = std::result::Result<Response<T>, Status>;

/// The shared SafetyService implementation for both transports.
#[derive(Clone)]
pub struct SafetyApi {
    agents: Arc<Agents>,
    base: ForgeServices,
}

fn denied(subcode: &str, message: &str) -> ForgeError {
    ForgeError::permission_denied(message).with_subcode(subcode)
}

fn require_person(caller: &Caller) -> Result<()> {
    if caller.cx.actor_kind == PrincipalKind::Human && !caller.is_anonymous() {
        Ok(())
    } else {
        Err(denied(
            "safety_person",
            "A signed-in person must make this request",
        ))
    }
}

fn required_text(field: &str, value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() {
        return Err(ForgeError::missing(field));
    }
    crate::agents::check_len(field, value, max)
}

fn verdict(value: i32) -> Result<SafetyVerdict> {
    SafetyVerdict::try_from(value)
        .ok()
        .filter(|v| *v != SafetyVerdict::Unspecified)
        .ok_or_else(|| ForgeError::invalid("safety_verdict", "Expected a safety verdict"))
}

fn finding<'a>(record: &'a mut SafetyRecord, id: &str) -> Result<&'a mut SafetyFinding> {
    record
        .findings
        .iter_mut()
        .find(|f| f.id == id)
        .ok_or_else(|| ForgeError::not_found("Safety finding").with_subcode("safety_finding"))
}

fn update_verdict(record: &mut SafetyRecord, unfinished: bool) {
    record.verdict = record.r#override.as_ref().map_or_else(
        || detectors::verdict(&record.findings, unfinished) as i32,
        |v| v.verdict,
    );
}

fn canonical(repo: &Repo, kind: SafetyWorkKind, id: String) -> SafetyRef {
    SafetyRef {
        repo: repo_ref_id(&repo.id),
        kind: kind as i32,
        id,
    }
}

impl SafetyApi {
    /// Constructs the service using the existing agent subsystem.
    pub fn new(agents: Arc<Agents>) -> Self {
        Self {
            base: ForgeServices::new(agents.forge.clone()),
            agents,
        }
    }

    /// The shared service, registered with gRPC and Connect.
    pub fn server(&self) -> SafetyServiceServer<Self> {
        SafetyServiceServer::new(self.clone())
    }

    async fn repo(&self, caller: &Caller, r: &Option<RepoRef>) -> Result<(Repo, Cx)> {
        let repo = self.base.repo(caller, r).await?;
        // Prompts, recorded commands and reports can refer to any repository path.
        // A credential that can read only part of a repository cannot see the aggregate.
        let cx = self
            .agents
            .forge
            .authz
            .repo_every_path(caller, &repo, Action::Read)
            .await?;
        Ok((repo, cx))
    }

    async fn target(
        &self,
        caller: &Caller,
        work: Option<SafetyRef>,
    ) -> Result<(Repo, Cx, SafetyRef)> {
        let work = work.ok_or_else(|| ForgeError::missing("work"))?;
        let (repo, cx) = self.repo(caller, &work.repo).await?;
        let kind = work.kind();
        if kind == SafetyWorkKind::Unspecified || work.id.is_empty() || work.id.contains('/') {
            return Err(ForgeError::invalid(
                "safety_work",
                "Expected a run or swarm ID",
            ));
        }
        let work = if kind == SafetyWorkKind::Run {
            evidence::work_for_run(&self.agents, &cx, &repo, &work.id).await?
        } else {
            canonical(&repo, kind, work.id)
        };
        Ok((repo, cx, work))
    }

    async fn writer(&self, caller: &Caller, repo: &Repo) -> Result<Cx> {
        self.agents
            .forge
            .authz
            .repo(
                caller,
                repo,
                Action::Commit,
                &[],
                None,
                Some(RepoRole::Write),
            )
            .await
    }

    async fn may_decide(&self, caller: &Caller, repo: &Repo, record: &SafetyRecord) -> Result<Cx> {
        require_person(caller)?;
        if record
            .answerable
            .as_ref()
            .is_some_and(|p| p.id == caller.cx.actor_id)
        {
            return self.writer(caller, repo).await;
        }
        self.agents
            .forge
            .authz
            .repo(
                caller,
                repo,
                Action::Commit,
                &[],
                None,
                Some(RepoRole::Admin),
            )
            .await
    }

    // Read or refresh derived evidence with a system actor. Audit entries for human
    // decisions are written separately under the authorized actor's context.
    pub(super) async fn refresh(
        &self,
        repo: &Repo,
        cx: &Cx,
        work: SafetyRef,
    ) -> Result<(SafetyRecord, bool)> {
        let snapshot = evidence::collect(&self.agents, cx, repo, work.clone()).await?;
        let mut snapshot_record = snapshot.record;
        let system = Cx::system().with_deadline(cx.deadline);
        store::ensure(&self.agents, &system, repo, &work).await?;
        let mut tx = self.agents.db.begin(SURFACE, &system).await?;
        let (mut record, row) = store::lock(&mut tx, repo, &work).await?;
        let new_record = record.work.is_none();
        let mut interruptions = Vec::new();
        let observed = snapshot_record.updated_at.as_ref().map_or(0, ts_ms);
        let previous = record.updated_at.as_ref().map_or(0, ts_ms);
        if new_record || observed >= previous {
            let new_findings = std::mem::take(&mut snapshot_record.findings);
            snapshot_record.findings = std::mem::take(&mut record.findings);
            snapshot_record.reviews = std::mem::take(&mut record.reviews);
            snapshot_record.r#override = record.r#override.take();
            let mut changed = new_record
                || record.asked != snapshot_record.asked
                || record.done != snapshot_record.done
                || record.agents != snapshot_record.agents
                || record.answerable != snapshot_record.answerable
                || observed != previous;
            for finding in new_findings {
                if snapshot_record.findings.iter().any(|f| f.id == finding.id) {
                    continue;
                }
                changed = true;
                interruptions.extend(
                    self.on_finding(&mut tx, repo, &work, &snapshot_record, &finding)
                        .await?,
                );
                snapshot_record.findings.push(finding);
            }
            update_verdict(&mut snapshot_record, snapshot.unfinished);
            if changed || record.verdict != snapshot_record.verdict {
                store::save(&mut tx, row, &snapshot_record).await?;
                store::emit(
                    &mut tx,
                    repo,
                    &work,
                    "safety.record_updated",
                    json!({"kind": work.kind().as_str_name()}),
                )
                .await?;
                tx.commit().await?;
                for interruption in interruptions {
                    interruption.finish(&self.agents).await;
                }
            }
            return Ok((snapshot_record, snapshot.unfinished));
        }
        Ok((record, snapshot.unfinished))
    }

    async fn get(&self, caller: &Caller, work: SafetyRef) -> Result<SafetyRecord> {
        let (repo, cx, work) = self.target(caller, Some(work)).await?;
        self.refresh(&repo, &cx, work)
            .await
            .map(|(record, _)| record)
    }

    async fn list(
        &self,
        caller: &Caller,
        request: ListSafetyRecordsRequest,
    ) -> Result<ListSafetyRecordsResponse> {
        let (repo, cx) = self.repo(caller, &request.repo).await?;
        let (after, size) = page(&request.page);
        // A record can weigh up to 384 KiB, including review history.
        let (refs, token) =
            evidence::list_refs(&self.agents, &cx, &repo, &after, size.min(7)).await?;
        let mut records = Vec::with_capacity(refs.len());
        for work in refs {
            records.push(self.refresh(&repo, &cx, work).await?.0);
        }
        Ok(ListSafetyRecordsResponse {
            records,
            page: next(token),
        })
    }

    async fn set_verdict(
        &self,
        caller: &Caller,
        request: SetVerdictRequest,
    ) -> Result<SafetyRecord> {
        let value = verdict(request.verdict)?;
        required_text("note", &request.note, 2000)?;
        let (repo, cx, work) = self.target(caller, request.work).await?;
        let (record, _) = self.refresh(&repo, &cx, work.clone()).await?;
        let auth = self.may_decide(caller, &repo, &record).await;
        let mut cx = self
            .base
            .guarded(caller, &repo.org_id, &repo.id, "safety.verdict_set", auth)
            .await?;
        cx.reason.clone_from(&request.note);
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let (mut record, row) = store::lock(&mut tx, &repo, &work).await?;
        record.verdict = value as i32;
        record.r#override = Some(SafetyVerdictOverride {
            verdict: value as i32,
            person: Some(caller.principal()),
            note: request.note.clone(),
            time: Some(ts(tx.now_ms())),
        });
        store::save(&mut tx, row, &record).await?;
        store::emit(
            &mut tx,
            &repo,
            &work,
            "safety.verdict_set",
            json!({"verdict": value.as_str_name()}),
        )
        .await?;
        tx.commit().await?;
        Ok(record)
    }

    async fn resolve_finding(
        &self,
        caller: &Caller,
        request: ResolveFindingRequest,
    ) -> Result<SafetyFinding> {
        let state = request.state();
        if !matches!(
            state,
            SafetyFindingState::Dismissed | SafetyFindingState::Confirmed
        ) {
            return Err(ForgeError::invalid(
                "safety_finding_state",
                "Confirm or dismiss the finding",
            ));
        }
        required_text("reason", &request.reason, 2000)?;
        let (repo, cx, work) = self.target(caller, request.work).await?;
        let (record, unfinished) = self.refresh(&repo, &cx, work.clone()).await?;
        let auth = self.may_decide(caller, &repo, &record).await;
        let mut cx = self
            .base
            .guarded(
                caller,
                &repo.org_id,
                &repo.id,
                "safety.finding_resolved",
                auth,
            )
            .await?;
        cx.reason.clone_from(&request.reason);
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let (mut record, row) = store::lock(&mut tx, &repo, &work).await?;
        let resolved = finding(&mut record, &request.finding_id)?;
        if resolved.state() != SafetyFindingState::Open {
            if resolved.state() == state
                && resolved.resolution_reason == request.reason
                && resolved
                    .resolved_by
                    .as_ref()
                    .is_some_and(|p| p.id == cx.actor_id)
            {
                return Ok(resolved.clone());
            }
            return Err(ForgeError::invalid(
                "safety_finding_resolved",
                "Finding already has a recorded decision",
            ));
        }

        resolved.state = state as i32;
        resolved.resolved_by = Some(caller.principal());
        resolved.resolved_at = Some(ts(tx.now_ms()));
        resolved.resolution_reason.clone_from(&request.reason);
        let result = resolved.clone();
        update_verdict(&mut record, unfinished);
        store::save(&mut tx, row, &record).await?;
        store::emit(
            &mut tx,
            &repo,
            &work,
            "safety.finding_resolved",
            json!({
                "finding_id": request.finding_id, "state": state.as_str_name(),
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn set_stop(
        &self,
        caller: &Caller,
        request: SetSafetyStopRequest,
        frozen: bool,
    ) -> Result<SafetyStop> {
        require_person(caller)?;
        required_text("reason", &request.reason, 2000)?;
        let org = self
            .agents
            .forge
            .orgs
            .get_org(&caller.cx, &request.org)
            .await?;
        let auth = self
            .agents
            .forge
            .authz
            .org(caller, &org.id, OrgPerm::Own)
            .await;
        let action = if frozen {
            "safety.stopped"
        } else {
            "safety.resumed"
        };
        let cx = self.base.guarded(caller, &org.id, "", action, auth).await?;
        control::set_stop(&self.agents, &cx, &org, frozen, &request.reason).await
    }
}

fn result<T>(caller: &Caller, response: Result<T>) -> Rpc<T> {
    reply(
        caller,
        response.map_err(|e| {
            if e.subcode.is_empty() {
                e.with_subcode("safety_request")
            } else {
                e
            }
        }),
    )
}

#[tonic::async_trait]
impl SafetyService for SafetyApi {
    async fn get_safety_record(&self, request: Request<SafetyRef>) -> Rpc<SafetyRecord> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::get(self, &caller, request.into_inner()).await,
        )
    }

    async fn list_safety_records(
        &self,
        request: Request<ListSafetyRecordsRequest>,
    ) -> Rpc<ListSafetyRecordsResponse> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::list(self, &caller, request.into_inner()).await,
        )
    }

    async fn report_finding(&self, request: Request<ReportFindingRequest>) -> Rpc<SafetyFinding> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::report_finding(self, &caller, request.into_inner()).await,
        )
    }

    async fn resolve_finding(&self, request: Request<ResolveFindingRequest>) -> Rpc<SafetyFinding> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::resolve_finding(self, &caller, request.into_inner()).await,
        )
    }

    async fn set_verdict(&self, request: Request<SetVerdictRequest>) -> Rpc<SafetyRecord> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::set_verdict(self, &caller, request.into_inner()).await,
        )
    }

    async fn request_safety_review(
        &self,
        request: Request<RequestSafetyReviewRequest>,
    ) -> Rpc<SafetyReview> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::request_review(self, &caller, request.into_inner()).await,
        )
    }

    async fn submit_safety_review(
        &self,
        request: Request<SubmitSafetyReviewRequest>,
    ) -> Rpc<SafetyReview> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::submit_review(self, &caller, request.into_inner()).await,
        )
    }

    async fn raise_concern(&self, request: Request<RaiseConcernRequest>) -> Rpc<SafetyConcern> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::raise_concern(self, &caller, request.into_inner()).await,
        )
    }

    async fn list_safety_attention(
        &self,
        request: Request<ListSafetyAttentionRequest>,
    ) -> Rpc<ListSafetyAttentionResponse> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::list_attention(self, &caller, request.into_inner()).await,
        )
    }

    async fn acknowledge_safety_attention(
        &self,
        request: Request<AcknowledgeSafetyAttentionRequest>,
    ) -> Rpc<SafetyAttention> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::acknowledge(self, &caller, request.into_inner()).await,
        )
    }

    async fn stop_everything(&self, request: Request<SetSafetyStopRequest>) -> Rpc<SafetyStop> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::set_stop(self, &caller, request.into_inner(), true).await,
        )
    }

    async fn resume_everything(&self, request: Request<SetSafetyStopRequest>) -> Rpc<SafetyStop> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::set_stop(self, &caller, request.into_inner(), false).await,
        )
    }

    async fn spot_check(&self, request: Request<SpotCheckRequest>) -> Rpc<SpotCheckResponse> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::spot_check(self, &caller, request.into_inner()).await,
        )
    }

    async fn get_safety_summary(
        &self,
        request: Request<GetSafetySummaryRequest>,
    ) -> Rpc<SafetySummary> {
        let caller = self.base.caller(&request)?;
        result(
            &caller,
            SafetyApi::summary(self, &caller, request.into_inner()).await,
        )
    }
}
