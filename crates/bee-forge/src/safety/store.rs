use bee_proto::v1::*;
use prost::Message as _;
use serde_json::Value;

use super::{ATTENTION, RECORDS};
use crate::agents::{Agents, SURFACE};
use crate::audit::AuditDraft;
use crate::cx::Cx;
use crate::error::{ForgeError, Result};
use crate::ids::ts_ms;
use crate::store::{Cursor, Row, Scan, Tx};

const MAX_RECORD_BYTES: usize = 384 * 1024;
pub(super) const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;

pub(super) fn key(work: &SafetyRef) -> String {
    format!("{}/{}", work.kind, work.id)
}

pub(super) async fn lock(
    tx: &mut Tx,
    repo: &Repo,
    work: &SafetyRef,
) -> Result<(SafetyRecord, Row)> {
    let row = tx
        .get_for_update(&RECORDS, &repo.id, &key(work))
        .await?
        .ok_or_else(|| ForgeError::not_found("Safety record").with_subcode("safety_record"))?;
    let mut record = row.decode()?;
    load_findings(tx, repo, &mut record).await?;
    Ok((record, row))
}

pub(super) async fn ensure(agents: &Agents, cx: &Cx, repo: &Repo, work: &SafetyRef) -> Result<()> {
    let id = key(work);
    let mut tx = agents.db.begin(SURFACE, cx).await?;
    if tx.get(&RECORDS, &repo.id, &id).await?.is_some() {
        return Ok(());
    }
    // Competing creators insert the same key. A duplicate ends this transaction on
    // Postgres, so the caller always opens a fresh transaction to lock the row.
    match tx.insert(&RECORDS, &Row::new(&repo.id, &id)).await {
        Ok(()) => tx.commit().await,
        Err(e) if e.reason == ErrorReason::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn finding_scope(repo: &str, work: &SafetyRef) -> String {
    format!("safety-findings/{repo}/{}", key(work))
}

const MAX_VISIBLE_FINDINGS: usize = 40;
const FINDINGS_OMITTED: &str = "omitted-findings";

pub(super) async fn load_findings(
    tx: &mut Tx,
    repo: &Repo,
    record: &mut SafetyRecord,
) -> Result<()> {
    let Some(work) = &record.work else {
        return Ok(());
    };
    if !record
        .findings
        .iter()
        .any(|finding| finding.id == FINDINGS_OMITTED)
    {
        return Ok(());
    }
    record
        .findings
        .retain(|finding| finding.id != FINDINGS_OMITTED);
    record.findings.extend(
        overflow(tx, &finding_scope(&repo.id, work))
            .await?
            .into_iter()
            .map(|(finding, _)| finding),
    );
    Ok(())
}

async fn overflow(tx: &mut Tx, scope: &str) -> Result<Vec<(SafetyFinding, Row)>> {
    let mut cursor = None;
    let mut findings = Vec::new();
    loop {
        let page = tx
            .scan_msgs(&RECORDS, &Scan::scope(scope, 128).after(cursor))
            .await?;
        cursor = page.last().map(|(_, row)| Cursor::after(row));
        let done = page.len() < 128;
        findings.extend(page);
        if done {
            return Ok(findings);
        }
    }
}

fn preview(record: &mut SafetyRecord, available: usize) -> Vec<SafetyFinding> {
    let mut bytes = 0;
    let count = record
        .findings
        .iter()
        .take(MAX_VISIBLE_FINDINGS)
        .take_while(|finding| {
            bytes += finding.encoded_len() + 8;
            bytes <= available
        })
        .count();
    let omitted = record.findings.split_off(count);
    if !omitted.is_empty() {
        record.findings.push(SafetyFinding {
            id: FINDINGS_OMITTED.into(),
            report: Some(SafetyFindingReport {
                evidence: Some(SafetyEvidence {
                    observation: format!("and {} more findings", omitted.len()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    omitted
}

pub(super) fn present(mut record: SafetyRecord) -> SafetyRecord {
    let budget = available_for_findings(&mut record);
    preview(&mut record, budget);
    record
}

fn available_for_findings(record: &mut SafetyRecord) -> usize {
    let findings = std::mem::take(&mut record.findings);
    let budget = MAX_RECORD_BYTES
        .saturating_sub(record.encoded_len() + 256)
        .min(144 * 1024);
    record.findings = findings;
    budget
}

pub(super) fn bound_evidence(record: &mut SafetyRecord) {
    if let Some(done) = record.done.as_mut() {
        bound_strings(&mut done.paths, 24 * 1024);
        bound_strings(&mut done.commands, 24 * 1024);
        bound_strings(&mut done.network_destinations, 8 * 1024);
        bound_strings(&mut done.merged_change_request_ids, 8 * 1024);
    }
    let mut asked_bytes = 0;
    let count = record
        .asked
        .iter()
        .take_while(|ask| {
            asked_bytes += ask.encoded_len() + 8;
            asked_bytes <= 64 * 1024
        })
        .count();
    let omitted = record.asked.len() - count;
    if omitted > 0 {
        record.asked.truncate(count);
        record.asked.push(SafetyAsk {
            text: format!("and {omitted} more requests"),
            source: None,
        });
    }
    for finding in &mut record.findings {
        if super::detectors::automatic(finding)
            && let Some(evidence) = finding
                .report
                .as_mut()
                .and_then(|report| report.evidence.as_mut())
        {
            bound_strings(&mut evidence.paths, 8 * 1024);
            if evidence.observation.len() > 4096 {
                let mut end = 4096;
                while !evidence.observation.is_char_boundary(end) {
                    end -= 1;
                }
                evidence.observation.truncate(end);
                evidence.observation.push('…');
            }
        }
    }
}

fn bound_strings(values: &mut Vec<String>, budget: usize) {
    let mut bytes = 0;
    let count = values
        .iter()
        .take_while(|value| {
            bytes += value.len() + 8;
            bytes <= budget
        })
        .count();
    let omitted = values.len() - count;
    if omitted > 0 {
        values.truncate(count);
        values.push(format!("and {omitted} more"));
    }
}

pub(super) async fn save(tx: &mut Tx, row: Row, record: &SafetyRecord) -> Result<()> {
    let mut stored = record.clone();
    let budget = available_for_findings(&mut stored);
    let omitted = preview(&mut stored, budget);
    let bytes = stored.encoded_len();
    if bytes > MAX_RECORD_BYTES {
        return Err(ForgeError::too_large(
            "safety review history",
            MAX_RECORD_BYTES as u64,
            bytes as u64,
        ));
    }
    if let Some(work) = record.work.as_ref() {
        let scope = finding_scope(&row.scope, work);
        let old = overflow(tx, &scope).await?;
        let mut remaining: std::collections::BTreeMap<_, _> = old
            .into_iter()
            .map(|(finding, row)| (row.id, finding))
            .collect();
        for finding in omitted {
            let previous = remaining.remove(&finding.id);
            if previous.as_ref() != Some(&finding) {
                tx.put(&RECORDS, &Row::new(&scope, &finding.id).msg(&finding))
                    .await?;
            }
        }
        for id in remaining.into_keys() {
            tx.delete(&RECORDS, &scope, &id).await?;
        }
    }
    let time = stored.updated_at.as_ref().map_or(0, ts_ms);
    tx.update(&RECORDS, &row.n1(time as i64).msg(&stored))
        .await?;
    Ok(())
}

const ADMIN_RECIPIENT: &str = "repository-administrators";

pub(super) fn attention_recipient(row: &Row, person: &str, repo_admin: bool) -> bool {
    row.k1.as_deref() == Some(person) || (repo_admin && row.k1.as_deref() == Some(ADMIN_RECIPIENT))
}

pub(super) async fn add_attention(tx: &mut Tx, repo: &Repo, item: &SafetyAttention) -> Result<()> {
    let mut item = item.clone();
    item.answerable = item
        .answerable
        .filter(|p| p.kind() == PrincipalKind::Human && !p.id.is_empty());
    // An unset answerable principal is the repository-admin inbox. It is one
    // shared item, so the first administrator to acknowledge it clears the item.
    let recipient = item
        .answerable
        .as_ref()
        .map_or(ADMIN_RECIPIENT, |p| p.id.as_str());
    let created = item.created_at.as_ref().map_or(0, ts_ms);
    tx.insert(
        &ATTENTION,
        &Row::new(&repo.id, &item.id)
            .k1(recipient)
            .n1(created as i64)
            .msg(&item),
    )
    .await
}

pub(super) async fn retire(tx: &mut Tx, repo: &Repo, work: &SafetyRef, id: &str) -> Result<()> {
    tx.audit(
        AuditDraft::new("safety.finding_retired", &repo.org_id)
            .repo_id(&repo.id)
            .target(SubjectKind::SafetyRecord, &work.id)
            .after(&serde_json::json!({ "finding_id": id }).to_string()),
    )
    .await
}

pub(super) async fn emit(
    tx: &mut Tx,
    repo: &Repo,
    work: &SafetyRef,
    action: &str,
    data: Value,
) -> Result<()> {
    let subject = SubjectRef {
        kind: SubjectKind::SafetyRecord as i32,
        repo: work.repo.clone(),
        id: work.id.clone(),
    };
    tx.emit_event(Event {
        kind: action.into(),
        repo_id: repo.id.clone(),
        org_id: repo.org_id.clone(),
        subject: Some(subject),
        payload_json: data.to_string(),
        ..Default::default()
    })
    .await?;
    tx.audit(
        AuditDraft::new(action, &repo.org_id)
            .repo_id(&repo.id)
            .target(SubjectKind::SafetyRecord, &work.id)
            .after(&data.to_string()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_preview_keeps_omission_counts_and_the_full_verdict() {
        let mut record = SafetyRecord {
            done: Some(SafetyDone {
                paths: vec!["a".repeat(16 * 1024), "b".repeat(16 * 1024)],
                commands: vec!["a".repeat(32 * 1024)],
                ..Default::default()
            }),
            verdict: SafetyVerdict::NeedsALook as i32,
            findings: (0..41)
                .map(|id| SafetyFinding {
                    id: id.to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        bound_evidence(&mut record);
        let preview = present(record);
        let done = preview.done.as_ref().unwrap();
        assert_eq!(done.paths.last().unwrap(), "and 1 more");
        assert_eq!(done.commands, ["and 1 more"]);
        assert_eq!(preview.verdict(), SafetyVerdict::NeedsALook);
        assert_eq!(preview.findings.len(), MAX_VISIBLE_FINDINGS + 1);
        assert_eq!(
            preview
                .findings
                .last()
                .unwrap()
                .report
                .as_ref()
                .unwrap()
                .evidence
                .as_ref()
                .unwrap()
                .observation,
            "and 1 more findings",
        );
        assert!(preview.encoded_len() < MAX_RECORD_BYTES);
    }

    #[test]
    fn fallback_attention_requires_repository_administration() {
        let fallback = Row::new("repo", "item").k1(ADMIN_RECIPIENT);
        let addressed = Row::new("repo", "item").k1("owner");
        assert!(attention_recipient(&fallback, "admin", true));
        assert!(!attention_recipient(&fallback, "reader", false));
        assert!(attention_recipient(&addressed, "owner", false));
        assert!(!attention_recipient(&addressed, "admin", true));
    }
}
