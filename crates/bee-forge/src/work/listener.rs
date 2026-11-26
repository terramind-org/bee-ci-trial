//! Reacts to change request events: development links from closing keywords, closing linked
//! issues when a change lands, finishing agent runs, and project automations for change
//! requests.
//!
//! The events come from the changes module's `cr.*` kinds with the `ChangeRequest` body
//! (`docs/spec/forge-core.md` §9). Every handler is idempotent: the event log delivers at
//! least once.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use bee_proto::v1::{
    ChangeRequest, Event, IssueLinkSource, IssueStateReason, Principal, PrincipalKind, Repo,
    RepoRef, Review, Verdict, event, repo_ref,
};

use super::automation::{ChangeTrigger, on_change};
use super::issues::{close_issue_in_tx, load_issue};
use super::model::{IssueRec, LinkRec};
use super::projects::automation_principal;
use super::relations::{ChangeLink, link_in_tx, unlink_in_tx};
use super::*;
use crate::events::EventListener;
use crate::orgs::Orgs;

/// An issue a change request names, resolved to a repo id and number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedIssue {
    /// Repo of the issue.
    pub repo_id: String,
    /// Its number.
    pub number: u64,
    /// Named by a closing keyword (else by `linked_issues`).
    pub by_keyword: bool,
}

fn change_state(kind: &str, cr: &ChangeRequest) -> &'static str {
    match kind {
        "cr.landed" => "landed",
        "cr.closed" => "closed",
        _ if cr.draft => "draft",
        _ => "open",
    }
}

fn author_of(cr: &ChangeRequest) -> Principal {
    cr.author.clone().unwrap_or_else(automation_principal)
}

impl Work {
    /// Applies one change request event. `named` are the issues the CR names (the listener
    /// resolves `owner/repo#N` references; same-repo numbers resolve to `repo.id`).
    pub async fn on_change_event(
        &self,
        cx: &Cx,
        repo: &Repo,
        kind: &str,
        cr: &ChangeRequest,
        named: &[NamedIssue],
    ) -> Result<()> {
        let info = RepoInfo::from(repo);
        let link = ChangeLink {
            id: cr.id.clone(),
            repo_id: repo.id.clone(),
            number: cr.number,
            title: cr.title.clone(),
            state: change_state(kind, cr).to_string(),
        };
        let author = author_of(cr);
        let mut tx = self.db.begin(SURFACE, cx).await?;
        note_repo(&mut tx, &info).await?;
        // Which issues the CR is tied to now.
        let mut wanted: Vec<(IssueRec, bool)> = Vec::new();
        if matches!(
            kind,
            "cr.opened" | "cr.edited" | "cr.revised" | "cr.reopened" | "cr.ready_for_review"
        ) {
            for n in named {
                if let Ok((rec, _)) = load_issue(&mut tx, &n.repo_id, n.number, false).await
                    && !wanted.iter().any(|(w, _)| w.id == rec.id)
                {
                    wanted.push((rec, n.by_keyword));
                }
            }
            for (rec, by_keyword) in &wanted {
                let agent_run = author.kind == PrincipalKind::Agent as i32
                    && rec
                        .active_agent_runs
                        .iter()
                        .any(|r| r.starts_with(&format!("{}/", author.id)));
                let source = if agent_run {
                    IssueLinkSource::AgentRun
                } else if *by_keyword {
                    IssueLinkSource::ClosingKeyword
                } else {
                    IssueLinkSource::Manual
                };
                let existed = tx.get(&LINKS, &rec.id, &cr.id).await?.is_some();
                link_in_tx(&mut tx, &rec.id, &author, &link, true, source).await?;
                if !existed {
                    let issue_repo = repo_hint(&mut tx, &rec.repo_id, &rec.org_id).await?;
                    record(
                        &mut tx,
                        Change {
                            kind: "issue.change_linked",
                            action: "issue.link_change",
                            org_id: &issue_repo.org_id,
                            repo_id: &issue_repo.id,
                            subject_kind: bee_proto::v1::SubjectKind::Issue,
                            subject_id: &rec.id,
                            payload: serde_json::json!({
                                "issue_id": rec.id, "number": rec.number,
                                "change_request_id": cr.id, "closes": true, "linked": true,
                            })
                            .to_string(),
                            after:
                                serde_json::json!({"number": rec.number, "change_request": cr.id})
                                    .to_string(),
                        },
                    )
                    .await?;
                }
            }
            // Keyword links the body no longer carries go away.
            let keep: BTreeSet<String> = wanted.iter().map(|(r, _)| r.id.clone()).collect();
            for r in scan_all(
                &mut tx,
                &LINKS,
                Scan {
                    key: Some(Key::K1(cr.id.clone())),
                    ..Scan::default()
                },
            )
            .await?
            {
                let l: LinkRec = r.decode()?;
                let kw = l
                    .change
                    .as_ref()
                    .is_some_and(|c| c.source == IssueLinkSource::ClosingKeyword as i32);
                if kw && !keep.contains(&r.scope) {
                    unlink_in_tx(&mut tx, &r.scope, &author, &cr.id).await?;
                }
            }
        }
        // Keep every link's display fields current.
        for r in scan_all(
            &mut tx,
            &LINKS,
            Scan {
                key: Some(Key::K1(cr.id.clone())),
                ..Scan::default()
            },
        )
        .await?
        {
            let mut l: LinkRec = r.decode()?;
            if let Some(c) = l.change.as_mut() {
                let before = c.clone();
                if !link.title.is_empty() {
                    c.title = link.title.clone();
                }
                c.state = link.state.clone();
                c.number = link.number;
                if *c != before {
                    tx.update(
                        &LINKS,
                        &Row::new(r.scope.clone(), r.id.clone())
                            .k1(cr.id.clone())
                            .version(r.version)
                            .msg(&l),
                    )
                    .await?;
                }
            }
        }
        let reviewers: Vec<String> = cr
            .review_requests
            .iter()
            .filter_map(|r| {
                r.reviewer.as_ref().map(|p| {
                    if p.display.is_empty() {
                        p.id.clone()
                    } else {
                        p.display.clone()
                    }
                })
            })
            .collect();
        super::items::refresh_change(&mut tx, &link, &author.id, &reviewers).await?;
        match kind {
            "cr.opened" => {
                super::automation::on_change_created(&mut tx, &info, &link, &author.id).await?;
            }
            "cr.reopened" => on_change(&mut tx, &cr.id, ChangeTrigger::Reopened).await?,
            "cr.landed" | "cr.closed" => {
                let landed = kind == "cr.landed";
                let default_branch = cr.target_ref.is_empty() || cr.target_ref == repo.default_ref;
                let links = scan_all(
                    &mut tx,
                    &LINKS,
                    Scan {
                        key: Some(Key::K1(cr.id.clone())),
                        ..Scan::default()
                    },
                )
                .await?;
                for r in links {
                    let l: LinkRec = r.decode()?;
                    let closes = l.change.as_ref().is_some_and(|c| c.closes);
                    if landed && closes && default_branch {
                        close_issue_in_tx(
                            &mut tx,
                            &r.scope,
                            &author,
                            IssueStateReason::Completed,
                            &cr.id,
                        )
                        .await?;
                    }
                    // The agent that opened the change is done with the issue.
                    if author.kind == PrincipalKind::Agent as i32
                        && let Some((mut rec, row)) =
                            super::issues::load_issue_id(&mut tx, &r.scope, true).await?
                    {
                        let old = rec.clone();
                        let rinfo = RepoInfo {
                            id: rec.repo_id.clone(),
                            org_id: rec.org_id.clone(),
                            full_name: repo_name(&mut tx, &rec.repo_id).await?,
                        };
                        let reason = if landed {
                            "change landed"
                        } else {
                            "change closed"
                        };
                        let n = super::assign::finish_runs(
                            &mut tx, &rinfo, &mut rec, &author, &author.id, "", reason,
                        )
                        .await?;
                        if n > 0 {
                            super::issues::save_issue(&mut tx, Some(&old), &mut rec, row.version)
                                .await?;
                        }
                    }
                }
                on_change(
                    &mut tx,
                    &cr.id,
                    if landed {
                        ChangeTrigger::Landed
                    } else {
                        ChangeTrigger::Closed
                    },
                )
                .await?;
            }
            _ => {}
        }
        tx.audit(
            crate::audit::AuditDraft::new("issue.change_event", &repo.org_id)
                .repo_id(&repo.id)
                .target(bee_proto::v1::SubjectKind::ChangeRequest, &cr.id)
                .after(&serde_json::json!({"event": kind, "number": cr.number, "issues": wanted.iter().map(|(r, _)| r.number).collect::<Vec<_>>()}).to_string()),
        )
        .await?;
        tx.commit().await
    }

    /// A review was submitted: project workflows for requested changes and approvals.
    pub async fn on_review_event(&self, cx: &Cx, review: &Review) -> Result<()> {
        if review.dismissed {
            return Ok(());
        }
        let trigger = match Verdict::try_from(review.verdict).unwrap_or(Verdict::Unspecified) {
            Verdict::Approve => ChangeTrigger::Approved,
            Verdict::RequestChanges => ChangeTrigger::ChangesRequested,
            _ => return Ok(()),
        };
        let mut tx = self.db.begin(SURFACE, cx).await?;
        on_change(&mut tx, &review.change_id, trigger).await?;
        tx.commit().await
    }
}

async fn repo_name(tx: &mut Tx, repo_id: &str) -> Result<String> {
    Ok(lookup_id(tx, "repo", repo_id)
        .await?
        .unwrap_or_else(|| placeholder(repo_id)))
}

/// Feeds `cr.*` and `review.submitted` events from the event log to [`Work`].
pub struct WorkListener {
    work: Arc<Work>,
    orgs: Arc<Orgs>,
}

impl WorkListener {
    /// A listener over `work`; `orgs` resolves repos named in closing keywords.
    pub fn new(work: Arc<Work>, orgs: Arc<Orgs>) -> WorkListener {
        WorkListener { work, orgs }
    }

    async fn named(&self, cx: &Cx, repo: &Repo, cr: &ChangeRequest) -> Vec<NamedIssue> {
        let mut out: Vec<NamedIssue> = Vec::new();
        let push = |n: NamedIssue, out: &mut Vec<NamedIssue>| {
            if !out
                .iter()
                .any(|x| x.repo_id == n.repo_id && x.number == n.number)
            {
                out.push(n);
            }
        };
        let mut refs = text::closing_references(&cr.body);
        for r in text::closing_references(&cr.title) {
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
        for r in refs {
            match r.repo {
                None => push(
                    NamedIssue {
                        repo_id: repo.id.clone(),
                        number: r.number,
                        by_keyword: true,
                    },
                    &mut out,
                ),
                Some(full) => {
                    let target = RepoRef {
                        repo: Some(repo_ref::Repo::FullName(full)),
                    };
                    if let Ok(t) = self.orgs.resolve_repo(cx, &target).await
                        && t.org_id == repo.org_id
                    {
                        push(
                            NamedIssue {
                                repo_id: t.id,
                                number: r.number,
                                by_keyword: true,
                            },
                            &mut out,
                        );
                    }
                }
            }
        }
        for n in &cr.linked_issues {
            push(
                NamedIssue {
                    repo_id: repo.id.clone(),
                    number: *n,
                    by_keyword: false,
                },
                &mut out,
            );
        }
        out
    }
}

#[async_trait]
impl EventListener for WorkListener {
    async fn on_event(&self, ev: &Event) {
        let cx = Cx::system();
        match (ev.kind.as_str(), &ev.body) {
            (
                "cr.opened"
                | "cr.edited"
                | "cr.revised"
                | "cr.reopened"
                | "cr.ready_for_review"
                | "cr.landed"
                | "cr.closed",
                Some(event::Body::Change(cr)),
            ) => {
                let repo_id = if ev.repo_id.is_empty() {
                    cr.repo_id.clone()
                } else {
                    ev.repo_id.clone()
                };
                let Ok(repo) = self.orgs.repo_any(&cx, &repo_id).await else {
                    return;
                };
                let named = self.named(&cx, &repo, cr).await;
                let _ = self
                    .work
                    .on_change_event(&cx, &repo, &ev.kind, cr, &named)
                    .await;
            }
            ("review.submitted", Some(event::Body::Review(r))) => {
                let _ = self.work.on_review_event(&cx, r).await;
            }
            _ => {}
        }
    }
}
