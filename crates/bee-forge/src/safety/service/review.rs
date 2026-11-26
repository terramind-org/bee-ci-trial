use super::*;

fn validate_report(report: &SafetyFindingReport) -> Result<()> {
    if report.kind() == SafetySignalKind::Unspecified
        || report.severity() == SafetySeverity::Unspecified
    {
        return Err(ForgeError::invalid(
            "safety_signal",
            "Expected a signal kind and severity",
        ));
    }
    let evidence = report
        .evidence
        .as_ref()
        .ok_or_else(|| ForgeError::missing("evidence"))?;
    required_text("evidence.observation", &evidence.observation, 4000)?;
    if evidence.paths.len() > 50 || evidence.subjects.len() > 50 {
        return Err(ForgeError::invalid(
            "safety_evidence_count",
            "Use at most 50 paths and 50 subjects per finding",
        ));
    }
    for path in &evidence.paths {
        required_text("evidence.paths", path, 1024)?;
    }
    for subject in &evidence.subjects {
        required_text("evidence.subjects.id", &subject.id, 256)?;
    }
    Ok(())
}

fn independent(record: &SafetyRecord, id: &str) -> Result<()> {
    if record.agents.iter().any(|agent| agent.id == id) {
        return Err(denied(
            "safety_self_review",
            "An agent cannot review its own work",
        ));
    }
    Ok(())
}

fn review_index(record: &SafetyRecord, caller: &Caller, review_id: &str) -> Result<usize> {
    independent(record, &caller.cx.actor_id)?;
    let index = record
        .reviews
        .iter()
        .position(|review| review.id == review_id)
        .ok_or_else(|| ForgeError::not_found("Safety review").with_subcode("safety_review"))?;
    let review = &record.reviews[index];
    if caller.cx.actor_kind != PrincipalKind::Agent
        || !review
            .reviewer
            .as_ref()
            .is_some_and(|p| p.id == caller.cx.actor_id)
    {
        return Err(denied(
            "safety_reviewer",
            "Only the assigned reviewer may submit evidence",
        ));
    }
    if review.submitted_at.is_some() {
        return Err(ForgeError::invalid(
            "safety_review_submitted",
            "The review has already been submitted",
        ));
    }
    Ok(index)
}

impl SafetyApi {
    async fn append_report(
        &self,
        tx: &mut Tx,
        repo: &Repo,
        work: &SafetyRef,
        record: &mut SafetyRecord,
        report: SafetyFindingReport,
    ) -> Result<(SafetyFinding, Option<control::Interruption>)> {
        let finding = SafetyFinding {
            id: tx.new_id()?,
            report: Some(report),
            state: SafetyFindingState::Open as i32,
            reported_by: Some(tx.cx().principal()),
            created_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        let stopped = self.on_finding(tx, repo, work, record, &finding).await?;
        record.findings.push(finding.clone());
        Ok((finding, stopped))
    }

    pub(super) async fn report_finding(
        &self,
        caller: &Caller,
        request: ReportFindingRequest,
    ) -> Result<SafetyFinding> {
        let report = request
            .report
            .ok_or_else(|| ForgeError::missing("report"))?;
        validate_report(&report)?;
        let (repo, cx, work) = self.target(caller, request.work).await?;
        let (_, unfinished) = self.refresh(&repo, &cx, work.clone()).await?;
        let auth = if request.review_id.is_empty() {
            require_person(caller)?;
            self.writer(caller, &repo).await
        } else {
            Ok(cx)
        };
        let cx = self
            .base
            .guarded(
                caller,
                &repo.org_id,
                &repo.id,
                "safety.finding_reported",
                auth,
            )
            .await?;
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let (mut record, row) = store::lock(&mut tx, &repo, &work).await?;
        let review = if request.review_id.is_empty() {
            None
        } else {
            Some(review_index(&record, caller, &request.review_id)?)
        };
        let (finding, stopped) = self
            .append_report(&mut tx, &repo, &work, &mut record, report)
            .await?;
        if let Some(i) = review {
            record.reviews[i].finding_ids.push(finding.id.clone());
        }
        update_verdict(&mut record, unfinished);
        store::save(&mut tx, row, &record).await?;
        tx.commit().await?;
        if let Some(stopped) = stopped {
            stopped.finish(&self.agents).await;
        }
        Ok(finding)
    }

    pub(super) async fn request_review(
        &self,
        caller: &Caller,
        request: RequestSafetyReviewRequest,
    ) -> Result<SafetyReview> {
        let (repo, cx, work) = self.target(caller, request.work).await?;
        let (record, _) = self.refresh(&repo, &cx, work.clone()).await?;
        let auth = self.may_decide(caller, &repo, &record).await;
        let cx = self
            .base
            .guarded(
                caller,
                &repo.org_id,
                &repo.id,
                "safety.review_requested",
                auth,
            )
            .await?;
        required_text("reviewer_id", &request.reviewer_id, 256)?;
        independent(&record, &request.reviewer_id)?;
        let mut reviewer = self
            .agents
            .definition(&cx, &repo.org_id, &request.reviewer_id)
            .await?
            .agent;
        reviewer.external_ids.clear();
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let (mut record, row) = store::lock(&mut tx, &repo, &work).await?;
        independent(&record, &reviewer.id)?;
        let review = SafetyReview {
            id: tx.new_id()?,
            requested_by: Some(caller.principal()),
            reviewer: Some(reviewer),
            requested_at: Some(ts(tx.now_ms())),
            ..Default::default()
        };
        record.reviews.push(review.clone());
        store::save(&mut tx, row, &record).await?;
        store::emit(&mut tx, &repo, &work, "safety.review_requested", json!({
            "review_id": review.id, "reviewer_id": request.reviewer_id, "kind": work.kind().as_str_name(),
        })).await?;
        tx.commit().await?;
        Ok(review)
    }

    pub(super) async fn submit_review(
        &self,
        caller: &Caller,
        request: SubmitSafetyReviewRequest,
    ) -> Result<SafetyReview> {
        let proposal = verdict(request.proposed_verdict)?;
        required_text("model", &request.model, 256)?;
        crate::agents::check_len("note", &request.note, 4000)?;
        if request.findings.len() > 20 {
            return Err(ForgeError::invalid(
                "safety_review_findings",
                "Submit at most 20 findings per review",
            ));
        }
        for report in &request.findings {
            validate_report(report)?;
        }
        let (repo, cx, work) = self.target(caller, request.work).await?;
        let (_, unfinished) = self.refresh(&repo, &cx, work.clone()).await?;
        let mut tx = self.agents.db.begin(SURFACE, &cx).await?;
        let (mut record, row) = store::lock(&mut tx, &repo, &work).await?;
        let i = review_index(&record, caller, &request.review_id)?;
        let mut interruptions = Vec::new();
        for report in request.findings {
            let (finding, stopped) = self
                .append_report(&mut tx, &repo, &work, &mut record, report)
                .await?;
            record.reviews[i].finding_ids.push(finding.id);
            interruptions.extend(stopped);
        }
        let review = &mut record.reviews[i];
        review.model = request.model;
        review.proposed_verdict = proposal as i32;
        review.note = request.note;
        review.submitted_at = Some(ts(tx.now_ms()));
        let review = review.clone();
        update_verdict(&mut record, unfinished);
        store::save(&mut tx, row, &record).await?;
        store::emit(
            &mut tx,
            &repo,
            &work,
            "safety.review_submitted",
            json!({
                "review_id": review.id, "model": review.model, "verdict": proposal.as_str_name(),
            }),
        )
        .await?;
        tx.commit().await?;
        for stopped in interruptions {
            stopped.finish(&self.agents).await;
        }
        Ok(review)
    }
}
