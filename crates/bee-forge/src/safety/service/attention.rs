use super::*;
use crate::agents::runs::terminal;
use crate::store::{Key, Scan};

impl SafetyApi {
    pub(super) async fn automatic_tripwire(
        &self,
        cx: &Cx,
        repo: &Repo,
        work: &SafetyRef,
        record: &SafetyRecord,
        finding: &SafetyFinding,
    ) -> Result<()> {
        let mut tx = self.agents.db.begin(SURFACE, cx).await?;
        // Serialize the check with evidence refresh, decisions, and other relay workers.
        let (current, _) = store::lock(&mut tx, repo, work).await?;
        if !current.findings.iter().any(|item| {
            item.id == finding.id
                && item.state() == SafetyFindingState::Open
                && item.report == finding.report
        }) || tx
            .get(&crate::safety::ATTENTION, &repo.id, &finding.id)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let stopped = self
            .on_finding(&mut tx, repo, work, record, finding)
            .await?;
        tx.commit().await?;
        if let Some(stopped) = stopped {
            stopped.finish(&self.agents).await;
        }
        Ok(())
    }

    // Finding and attention item share the cancellation transaction.
    pub(super) async fn on_finding(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        work: &SafetyRef,
        record: &SafetyRecord,
        finding: &SafetyFinding,
    ) -> Result<Option<control::Interruption>> {
        let Some(report) = &finding.report else {
            return Ok(None);
        };
        let kind = report.kind();
        if !detectors::automatic(finding) {
            store::emit(
                tx,
                repo,
                work,
                "safety.finding_reported",
                json!({
                    "finding_id": finding.id, "kind": kind.as_str_name(),
                    "severity": report.severity().as_str_name(),
                }),
            )
            .await?;
        }
        if report.severity() != SafetySeverity::Severe
            || !matches!(
                kind,
                SafetySignalKind::OwnLeash
                    | SafetySignalKind::SecretsAndExit
                    | SafetySignalKind::HarmToOthers
            )
        {
            return Ok(None);
        }

        let reason = report
            .evidence
            .as_ref()
            .map_or("", |e| e.observation.as_str());
        // Attention is visible to the recipient even when the audit log is restricted.
        let reason = if detectors::automatic(finding) && kind == SafetySignalKind::OwnLeash {
            "Changed repository policy, agent instructions, or governing settings"
        } else {
            reason
        };
        let stopped = control::interrupt_in_tx(
            &self.agents,
            tx,
            repo,
            work.kind(),
            &work.id,
            "A safety tripwire interrupted the work",
        )
        .await?;
        let item = SafetyAttention {
            id: if detectors::automatic(finding) {
                finding.id.clone()
            } else {
                tx.new_id()?
            },
            work: Some(work.clone()),
            answerable: record.answerable.clone(),
            created_at: Some(ts(tx.now_ms())),
            tripwire: Some(SafetyTripwire {
                finding_id: finding.id.clone(),
                kind: report.kind,
                reason: reason.into(),
                stopped_run_ids: stopped.stopped_run_ids(),
                paused_swarm: stopped.paused_swarm(),
            }),
            ..Default::default()
        };
        store::add_attention(tx, repo, &item).await?;
        store::emit(
            tx,
            repo,
            work,
            "safety.tripwire",
            json!({
                "attention_id": item.id, "finding_id": finding.id,
            }),
        )
        .await?;
        Ok(Some(stopped))
    }

    pub(super) async fn raise_concern(
        &self,
        caller: &Caller,
        request: RaiseConcernRequest,
    ) -> Result<SafetyConcern> {
        required_text("text", &request.text, 4000)?;
        if request.category() == SafetyConcernCategory::Unspecified {
            return Err(ForgeError::invalid(
                "safety_concern_category",
                "Expected a concern category",
            ));
        }
        let repo = self.base.repo(caller, &request.repo).await?;
        let cx = self
            .agents
            .forge
            .authz
            .repo(caller, &repo, Action::Read, &[], None, None)
            .await?;
        let run = self.agents.run_rec(&cx, &request.run_id).await?;
        let actual = run.run();
        if caller.cx.actor_kind != PrincipalKind::Agent
            || actual.repo != repo_ref_id(&repo.id)
            || actual
                .agent
                .as_ref()
                .is_none_or(|a| a.id != caller.cx.actor_id)
            || run.token_id.is_empty()
            || !caller
                .token
                .block_ids()
                .iter()
                .any(|id| id.0 == run.token_id)
        {
            return Err(denied(
                "safety_concern_run",
                "Use the credential issued for this run",
            ));
        }
        if terminal(actual.state) {
            return Err(ForgeError::invalid(
                "safety_concern_inactive",
                "The run has already ended",
            ));
        }
        let work = evidence::work_for_run(&self.agents, &cx, &repo, &request.run_id).await?;
        // A concern can interrupt without refreshing findings. It is never a detector input.
        let answerable = evidence::answerable_for_work(&self.agents, &cx, &repo, &work).await?;
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let concern = SafetyConcern {
            id: tx.new_id()?,
            run_id: request.run_id,
            category: request.category,
            text: request.text,
            raised_by: Some(caller.principal()),
        };
        let stopped = control::interrupt_in_tx(
            &self.agents,
            &mut tx,
            &repo,
            SafetyWorkKind::Run,
            &concern.run_id,
            "Agent raised a concern",
        )
        .await?;
        if stopped.stopped_run_ids().is_empty() {
            return Err(ForgeError::invalid(
                "safety_concern_inactive",
                "The run has already ended",
            ));
        }
        let item = SafetyAttention {
            id: concern.id.clone(),
            work: Some(work.clone()),
            answerable,
            concern: Some(concern.clone()),
            created_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        store::add_attention(&mut tx, &repo, &item).await?;
        store::emit(
            &mut tx,
            &repo,
            &work,
            "safety.concern_raised",
            json!({
                "attention_id": item.id, "run_id": concern.run_id,
                "category": concern.category().as_str_name(),
            }),
        )
        .await?;
        tx.commit().await?;
        stopped.finish(&self.agents).await;
        Ok(concern)
    }

    async fn may_receive_fallback(&self, caller: &Caller, repo: &Repo) -> bool {
        self.agents
            .forge
            .authz
            .repo(
                caller,
                repo,
                Action::ManageSettings,
                &[],
                None,
                Some(RepoRole::Admin),
            )
            .await
            .is_ok()
    }

    pub(super) async fn list_attention(
        &self,
        caller: &Caller,
        request: ListSafetyAttentionRequest,
    ) -> Result<ListSafetyAttentionResponse> {
        require_person(caller)?;
        let (repo, cx) = self.repo(caller, &request.repo).await?;
        let (after, size) = page(&request.page);
        let repo_admin = self.may_receive_fallback(caller, &repo).await;
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let mut cursor = None;
        let mut items = Vec::new();
        let mut token = String::new();
        loop {
            let mut scan = Scan::scope(&repo.id, 128).after(cursor);
            if !repo_admin {
                scan = scan.key(Key::K1(cx.actor_id.clone()));
            }
            let rows = tx.scan(&crate::safety::ATTENTION, &scan).await?;
            cursor = rows.last().map(crate::store::Cursor::after);
            let done = rows.len() < 128;
            for row in rows {
                if row.id <= after || !store::attention_recipient(&row, &cx.actor_id, repo_admin) {
                    continue;
                }
                let item: SafetyAttention = row.decode()?;
                if item.acknowledged_at.is_some() {
                    continue;
                }
                if items.len() == size {
                    return Ok(ListSafetyAttentionResponse {
                        items,
                        page: next(token),
                    });
                }
                token = row.id;
                items.push(item);
            }
            if done {
                return Ok(ListSafetyAttentionResponse {
                    items,
                    page: next(String::new()),
                });
            }
        }
    }

    pub(super) async fn acknowledge(
        &self,
        caller: &Caller,
        request: AcknowledgeSafetyAttentionRequest,
    ) -> Result<SafetyAttention> {
        require_person(caller)?;
        let (repo, cx) = self.repo(caller, &request.repo).await?;
        let repo_admin = self.may_receive_fallback(caller, &repo).await;
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let row = tx
            .get_for_update(&crate::safety::ATTENTION, &repo.id, &request.id)
            .await?
            .filter(|row| store::attention_recipient(row, &cx.actor_id, repo_admin))
            .ok_or_else(|| {
                ForgeError::not_found("Safety attention item").with_subcode("safety_attention")
            })?;
        let mut item: SafetyAttention = row.decode()?;
        if item.acknowledged_at.is_some() {
            return Ok(item);
        }
        item.acknowledged_at = Some(ts(tx.now_ms()));
        let work = item
            .work
            .as_ref()
            .ok_or_else(|| ForgeError::internal("Safety attention has no work"))?;
        tx.update(&crate::safety::ATTENTION, &row.msg(&item))
            .await?;
        store::emit(
            &mut tx,
            &repo,
            work,
            "safety.attention_acknowledged",
            json!({"attention_id": item.id}),
        )
        .await?;
        tx.commit().await?;
        Ok(item)
    }
}
