//! `IssueService` and `ProjectService` called in-process through their tonic traits with
//! tokens of different principals: the permission matrix, issue lifecycle, planning objects,
//! projects and automations, and the two agent scenarios of PRODUCT §4.15 at domain level
//! (`as_issue_01_*`, `as_issue_02_*`).

use crate::common;
use std::sync::Arc;

use async_trait::async_trait;
use bee_auth::Scope;
use bee_forge::Cx;
use bee_forge::audit::org_scope;
use bee_forge::error::Result as FResult;
use bee_forge::events::repo_log;
use bee_forge::service::ForgeServices;
use bee_forge::work::{
    AgentGate, NumberKind, TemplateFile, TemplateFiles, TemplateSource, WorkHooks,
};
use bee_policy::Action;
use bee_proto::v1::issue_service_server::IssueService;
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::project_service_server::ProjectService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::*;
use common::{Env, actions, reason, req};

struct Toks {
    owner: String,
    admin: String,
    writer: String,
    reader: String,
    outsider: String,
    agent: String,
}

struct W {
    env: Env,
    s: ForgeServices,
    repo: Repo,
    org: Org,
    t: Toks,
}

fn rref(full: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(full.into())),
    })
}

fn iref(full: &str, number: u64) -> Option<IssueRef> {
    Some(IssueRef {
        repo: rref(full),
        number,
        id: String::new(),
    })
}

async fn world_on(env: Env) -> W {
    let s = ForgeServices::new(env.forge.clone());
    let owner = env.full("owner");
    let org = s
        .create_org(req(
            &owner,
            CreateOrgRequest {
                name: "acme".into(),
                display: "Acme".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let repo = s
        .create_repo(req(
            &owner,
            CreateRepoRequest {
                org: "acme".into(),
                name: "app".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    s.create_repo(req(
        &owner,
        CreateRepoRequest {
            org: "acme".into(),
            name: "tools".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    for (who, role) in [
        ("admin", Role::Admin),
        ("writer", Role::Member),
        ("reader", Role::Member),
        ("agent", Role::Member),
    ] {
        s.set_member(req(
            &owner,
            SetMemberRequest {
                org: "acme".into(),
                principal_id: who.into(),
                role: role as i32,
            },
        ))
        .await
        .unwrap();
    }
    for repo_name in ["acme/app", "acme/tools"] {
        for who in ["writer", "agent"] {
            s.add_collaborator(req(
                &owner,
                AddCollaboratorRequest {
                    repo: rref(repo_name),
                    principal_id: who.into(),
                    role: RepoRole::Write as i32,
                    ..Default::default()
                },
            ))
            .await
            .unwrap();
        }
    }
    let t = Toks {
        owner: owner.clone(),
        admin: env.full("admin"),
        writer: env.full("writer"),
        reader: env.full("reader"),
        outsider: env.full("outsider"),
        agent: env.agent_token("owner", Scope::new()),
    };
    W {
        env,
        s,
        repo,
        org,
        t,
    }
}

async fn world() -> W {
    world_on(common::env().await).await
}

async fn open(w: &W, tok: &str, title: &str) -> Issue {
    w.s.create_issue(req(
        tok,
        CreateIssueRequest {
            repo: rref("acme/app"),
            title: title.into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn get(w: &W, tok: &str, number: u64) -> Issue {
    w.s.get_issue_by_number(req(
        tok,
        GetIssueByNumberRequest {
            repo: rref("acme/app"),
            number,
            include_html: false,
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn close(w: &W, tok: &str, number: u64, reason: IssueStateReason) -> Issue {
    w.s.close_issue(req(
        tok,
        CloseIssueRequest {
            repo: rref("acme/app"),
            number,
            reason: reason as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn list(w: &W, tok: &str, query: &str) -> Vec<u64> {
    let r =
        w.s.list_issues(req(
            tok,
            ListIssuesRequest {
                repo: rref("acme/app"),
                query: query.into(),
                all_states: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mut v: Vec<u64> = r.issues.iter().map(|i| i.number).collect();
    v.sort();
    assert_eq!(r.total_count as usize, v.len());
    v
}

async fn timeline(w: &W, number: u64) -> Vec<IssueTimelineEvent> {
    w.s.list_issue_timeline(req(
        &w.t.owner,
        ListIssueTimelineRequest {
            repo: rref("acme/app"),
            number,
            ..Default::default()
        },
    ))
    .await
    .unwrap()
    .into_inner()
    .events
}

fn kinds(events: &[IssueTimelineEvent]) -> Vec<IssueTimelineEventKind> {
    events
        .iter()
        .map(|e| IssueTimelineEventKind::try_from(e.kind).unwrap())
        .collect()
}

async fn event_kinds(w: &W) -> Vec<String> {
    w.env.forge.flush().await.unwrap();
    let (events, _) = w
        .env
        .forge
        .events
        .read(&Cx::system(), &repo_log(&w.repo.id), 0, &[], 1000)
        .await
        .unwrap();
    events.into_iter().map(|e| e.kind).collect()
}

async fn audit_entries(w: &W) -> Vec<(String, String)> {
    w.env.forge.flush().await.unwrap();
    let (entries, _) = w
        .env
        .forge
        .audit
        .query(
            &Cx::system(),
            &org_scope(&w.org.id),
            &AuditLogFilter::default(),
            true,
            "",
            500,
        )
        .await
        .unwrap();
    entries.into_iter().map(|e| (e.action, e.result)).collect()
}

// ---------------------------------------------------------------- issues

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_lifecycle_numbers_and_close_reasons() {
    let w = world().await;
    let a = open(&w, &w.t.reader, "First").await;
    assert_eq!((a.number, a.version), (1, 1));
    assert_eq!(a.author_association, "MEMBER");
    assert_eq!(a.repo_full_name, "acme/app");
    assert_eq!(a.url, "/acme/app/issues/1");
    let b = open(&w, &w.t.writer, "Second").await;
    assert_eq!(b.number, 2);
    // A change request takes number 3 from the same space; the next issue is 4.
    let n = w
        .env
        .forge
        .work
        .allocate_number(&Cx::system(), &w.repo.id, NumberKind::ChangeRequest, "CR1")
        .await
        .unwrap();
    assert_eq!(n, 3);
    let c = open(&w, &w.t.reader, "Third").await;
    assert_eq!(c.number, 4);
    assert_eq!(
        reason(
            w.s.get_issue_by_number(req(
                &w.t.reader,
                GetIssueByNumberRequest {
                    repo: rref("acme/app"),
                    number: 3,
                    include_html: false
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // By id.
    let by_id =
        w.s.get_issue(req(
            &w.t.reader,
            GetIssueRequest {
                id: a.id.clone(),
                include_html: false,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_id.title, "First");

    // Edits are versioned; a stale version is VERSION_MISMATCH.
    let upd = |tok: &str, number: u64, title: &str, version: u64| {
        w.s.update_issue(req(
            tok,
            UpdateIssueRequest {
                repo: rref("acme/app"),
                number,
                title: Some(title.into()),
                body: None,
                expected_version: version,
            },
        ))
    };
    let a2 = upd(&w.t.reader, 1, "First, renamed", 1)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a2.version, 2);
    assert_eq!(
        reason(upd(&w.t.reader, 1, "x", 1).await),
        "VERSION_MISMATCH"
    );
    // Only the author or a maintainer edits.
    assert_eq!(
        reason(upd(&w.t.reader, 2, "mine now", 0).await),
        "PERMISSION_DENIED"
    );
    assert!(upd(&w.t.writer, 1, "triaged title", 0).await.is_ok());
    let tl = timeline(&w, 1).await;
    let renames: Vec<_> = tl
        .iter()
        .filter(|e| e.kind == IssueTimelineEventKind::Renamed as i32)
        .collect();
    assert_eq!(renames.len(), 2);
    assert_eq!(renames[0].previous_title, "First");
    assert_eq!(renames[1].current_title, "triaged title");

    // Close reasons.
    let closed = close(&w, &w.t.reader, 1, IssueStateReason::Unspecified).await;
    assert_eq!(closed.state, IssueState::Closed as i32);
    assert_eq!(closed.state_reason, IssueStateReason::Completed as i32);
    assert!(closed.closed_at.is_some());
    assert_eq!(closed.closed_by.unwrap().id, "reader");
    assert_eq!(
        reason(
            w.s.close_issue(req(
                &w.t.reader,
                CloseIssueRequest {
                    repo: rref("acme/app"),
                    number: 1,
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    let np = close(&w, &w.t.writer, 2, IssueStateReason::NotPlanned).await;
    assert_eq!(np.state_reason, IssueStateReason::NotPlanned as i32);
    // DUPLICATE needs duplicate_of.
    let dup_req = |dup: Option<IssueRef>| CloseIssueRequest {
        repo: rref("acme/app"),
        number: 4,
        reason: IssueStateReason::Duplicate as i32,
        duplicate_of: dup,
        ..Default::default()
    };
    assert_eq!(
        reason(w.s.close_issue(req(&w.t.writer, dup_req(None))).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(
            w.s.close_issue(req(&w.t.writer, dup_req(iref("acme/app", 4))))
                .await
        ),
        "INVALID_ARGUMENT"
    );
    let d =
        w.s.close_issue(req(&w.t.writer, dup_req(iref("acme/app", 1))))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(d.state_reason, IssueStateReason::Duplicate as i32);
    assert_eq!(d.duplicate_of.unwrap().number, 1);
    assert!(kinds(&timeline(&w, 4).await).contains(&IssueTimelineEventKind::MarkedDuplicate));
    // Unmark and reopen.
    let u =
        w.s.unmark_issue_duplicate(req(
            &w.t.writer,
            UnmarkIssueDuplicateRequest {
                repo: rref("acme/app"),
                number: 4,
                reopen: true,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(u.state, IssueState::Open as i32);
    assert!(u.duplicate_of.is_none());
    assert_eq!(
        reason(
            w.s.reopen_issue(req(
                &w.t.writer,
                ReopenIssueRequest {
                    repo: rref("acme/app"),
                    number: 4,
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    let re =
        w.s.reopen_issue(req(
            &w.t.reader,
            ReopenIssueRequest {
                repo: rref("acme/app"),
                number: 1,
                comment: "still broken".into(),
                expected_version: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(re.state, IssueState::Open as i32);
    assert_eq!(re.comment_count, 1);
    // MarkIssueDuplicate on an open issue closes it.
    let m =
        w.s.mark_issue_duplicate(req(
            &w.t.writer,
            MarkIssueDuplicateRequest {
                repo: rref("acme/app"),
                number: 4,
                duplicate_of: iref("acme/app", 2),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(m.state, IssueState::Closed as i32);
    assert_eq!(m.duplicate_of.unwrap().number, 2);

    // Deleting needs a repo admin, and the number is never reused.
    let del = |tok: &str, number: u64| {
        w.s.delete_issue(req(
            tok,
            DeleteIssueRequest {
                repo: rref("acme/app"),
                number,
            },
        ))
    };
    assert_eq!(reason(del(&w.t.writer, 2).await), "PERMISSION_DENIED");
    del(&w.t.admin, 2).await.unwrap();
    assert_eq!(
        reason(
            w.s.get_issue_by_number(req(
                &w.t.admin,
                GetIssueByNumberRequest {
                    repo: rref("acme/app"),
                    number: 2,
                    include_html: false
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(open(&w, &w.t.reader, "Fifth").await.number, 5);
    // Issue 4's duplicate target vanished; reading it still works.
    assert_eq!(get(&w, &w.t.reader, 4).await.number, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_matrix_for_issues() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let issue = open(&w, &t.reader, "Mine").await;
    let writer_read_only = w.env.token("writer", actions(&[Action::Read]));
    let label = |tok: &str| {
        s.add_issue_labels(req(
            tok,
            AddIssueLabelsRequest {
                repo: rref("acme/app"),
                number: 1,
                labels: vec!["bug".into()],
                create_missing: false,
            },
        ))
    };
    // Readers open issues and comment; triage actions need the write role.
    assert_eq!(reason(label(&t.reader).await), "PERMISSION_DENIED");
    assert_eq!(reason(label(&t.outsider).await), "NOT_FOUND");
    assert_eq!(reason(label(&writer_read_only).await), "POLICY_DENIED");
    let labeled = label(&t.writer).await.unwrap().into_inner();
    assert_eq!(labeled.labels[0].name, "bug");
    // Reads: any reader, a read-only token, not outsiders.
    assert_eq!(get(&w, &writer_read_only, 1).await.number, 1);
    assert_eq!(
        reason(
            s.get_issue(req(
                &t.outsider,
                GetIssueRequest {
                    id: issue.id.clone(),
                    include_html: false
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // A reader may not set planning fields while opening an issue.
    let planning = s
        .create_issue(req(
            &t.reader,
            CreateIssueRequest {
                repo: rref("acme/app"),
                title: "x".into(),
                labels: vec!["bug".into()],
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(planning).await_none(), "PERMISSION_DENIED");
    // Maintainers' actions.
    for (name, result) in [
        (
            "lock",
            s.lock_issue(req(
                &t.reader,
                LockIssueRequest {
                    repo: rref("acme/app"),
                    number: 1,
                    reason: IssueLockReason::Spam as i32,
                },
            ))
            .await
            .map(|_| ()),
        ),
        (
            "pin",
            s.pin_issue(req(
                &t.reader,
                PinIssueRequest {
                    repo: rref("acme/app"),
                    number: 1,
                },
            ))
            .await
            .map(|_| ()),
        ),
        (
            "milestone",
            s.create_issue_milestone(req(
                &t.reader,
                CreateIssueMilestoneRequest {
                    repo: rref("acme/app"),
                    title: "m".into(),
                    ..Default::default()
                },
            ))
            .await
            .map(|_| ()),
        ),
        (
            "assign",
            s.add_issue_assignees(req(
                &t.reader,
                AddIssueAssigneesRequest {
                    repo: rref("acme/app"),
                    number: 1,
                    assignees: vec!["writer".into()],
                    ..Default::default()
                },
            ))
            .await
            .map(|_| ()),
        ),
    ] {
        assert_eq!(
            reason(result.map(tonic::Response::new)),
            "PERMISSION_DENIED",
            "{name}"
        );
    }
    // The author closes their own issue; a stranger's issue is a maintainer's call.
    let other = open(&w, &t.writer, "Writer's").await;
    assert_eq!(
        reason(
            s.close_issue(req(
                &t.reader,
                CloseIssueRequest {
                    repo: rref("acme/app"),
                    number: other.number,
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    close(&w, &t.reader, 1, IssueStateReason::Completed).await;
    // Comments and a lock.
    let comment = |tok: &str, body: &str| {
        s.create_issue_comment(req(
            tok,
            CreateIssueCommentRequest {
                repo: rref("acme/app"),
                number: 2,
                body: body.into(),
                ..Default::default()
            },
        ))
    };
    assert!(comment(&t.reader, "hello").await.is_ok());
    s.lock_issue(req(
        &t.writer,
        LockIssueRequest {
            repo: rref("acme/app"),
            number: 2,
            reason: IssueLockReason::TooHeated as i32,
        },
    ))
    .await
    .unwrap();
    assert_eq!(reason(comment(&t.reader, "again").await), "LOCKED");
    assert!(comment(&t.writer, "maintainers may").await.is_ok());
    assert_eq!(
        reason(
            s.lock_issue(req(
                &t.writer,
                LockIssueRequest {
                    repo: rref("acme/app"),
                    number: 2,
                    reason: 0
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    s.unlock_issue(req(
        &t.writer,
        UnlockIssueRequest {
            repo: rref("acme/app"),
            number: 2,
        },
    ))
    .await
    .unwrap();
    assert!(comment(&t.reader, "again").await.is_ok());

    // Denied state changes are audited with the reason.
    let audit = audit_entries(&w).await;
    assert!(
        audit
            .iter()
            .any(|(a, r)| a == "issue.label" && r == "PERMISSION_DENIED")
    );
    assert!(audit.iter().any(|(a, r)| a == "issue.create" && r == "ok"));
    assert!(audit.iter().any(|(a, r)| a == "issue.lock" && r == "ok"));

    // Archived repos and disabled issues.
    s.update_repo_settings(req(
        &t.owner,
        UpdateRepoSettingsRequest {
            repo: rref("acme/app"),
            has_issues: Some(false),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let blocked = s
        .create_issue(req(
            &t.writer,
            CreateIssueRequest {
                repo: rref("acme/app"),
                title: "no".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(blocked).await_none(), "LOCKED");
    assert_eq!(get(&w, &t.reader, 2).await.number, 2);
    s.update_repo_settings(req(
        &t.owner,
        UpdateRepoSettingsRequest {
            repo: rref("acme/app"),
            has_issues: Some(true),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    s.archive_repo(req(
        &t.owner,
        ArchiveRepoRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    let archived = s
        .create_issue(req(
            &t.writer,
            CreateIssueRequest {
                repo: rref("acme/app"),
                title: "no".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(archived).await_none(), "ARCHIVED");
}

/// `reason` wants a `Result<Response<_>, Status>`; creation results are `Response<Issue>`.
trait AwaitNone {
    fn await_none(self) -> Self;
}

impl<T> AwaitNone for T {
    fn await_none(self) -> T {
        self
    }
}

// ------------------------------------------------------------- labels etc.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn labels_milestones_and_task_lists() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    // The default set appears on first use.
    let labels = s
        .list_issue_labels(req(
            &t.reader,
            ListIssueLabelsRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .labels;
    assert!(
        labels
            .iter()
            .any(|l| l.name == "bug" && l.is_default && l.color == "d73a4a")
    );
    // Create, recolor, case-insensitive uniqueness, unknown labels.
    let create = |name: &str, color: &str| {
        s.create_issue_label(req(
            &t.writer,
            CreateIssueLabelRequest {
                repo: rref("acme/app"),
                name: name.into(),
                color: color.into(),
                description: "d".into(),
            },
        ))
    };
    create("area/api", "#00ff00").await.unwrap();
    assert_eq!(reason(create("AREA/API", "").await), "ALREADY_EXISTS");
    assert_eq!(reason(create("bad", "zzz").await), "INVALID_ARGUMENT");
    let upd = s
        .update_issue_label(req(
            &t.writer,
            UpdateIssueLabelRequest {
                repo: rref("acme/app"),
                name: "area/api".into(),
                new_name: Some("area/rest".into()),
                color: Some("112233".into()),
                description: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (upd.name.as_str(), upd.color.as_str()),
        ("area/rest", "112233")
    );
    let i = open(&w, &t.reader, "labelled").await;
    let add = |names: &[&str], create_missing: bool| {
        s.add_issue_labels(req(
            &t.writer,
            AddIssueLabelsRequest {
                repo: rref("acme/app"),
                number: i.number,
                labels: names.iter().map(|n| n.to_string()).collect(),
                create_missing,
            },
        ))
    };
    assert_eq!(reason(add(&["nope"], false).await), "NOT_FOUND");
    let got = add(&["Area/REST", "nope"], true)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.labels.len(), 2);
    // Renaming keeps the label on the issue; deleting removes it.
    s.update_issue_label(req(
        &t.writer,
        UpdateIssueLabelRequest {
            repo: rref("acme/app"),
            name: "area/rest".into(),
            new_name: Some("api".into()),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert!(
        get(&w, &t.reader, 1)
            .await
            .labels
            .iter()
            .any(|l| l.name == "api")
    );
    s.delete_issue_label(req(
        &t.writer,
        DeleteIssueLabelRequest {
            repo: rref("acme/app"),
            name: "api".into(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(get(&w, &t.reader, 1).await.labels.len(), 1);
    let set = s
        .set_issue_labels(req(
            &t.writer,
            SetIssueLabelsRequest {
                repo: rref("acme/app"),
                number: 1,
                labels: vec!["bug".into(), "question".into()],
                expected_version: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(set.labels.len(), 2);
    let removed = s
        .remove_issue_labels(req(
            &t.writer,
            RemoveIssueLabelsRequest {
                repo: rref("acme/app"),
                number: 1,
                labels: vec!["bug".into(), "never-had".into()],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(removed.labels.len(), 1);

    // Milestones: progress follows state, deletion clears the issue.
    let m = s
        .create_issue_milestone(req(
            &t.writer,
            CreateIssueMilestoneRequest {
                repo: rref("acme/app"),
                title: "v1".into(),
                description: "first".into(),
                due_on: Some(pbjson_types::Timestamp {
                    seconds: 4_000_000_000,
                    nanos: 0,
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(m.number, 1);
    let dup = s
        .create_issue_milestone(req(
            &t.writer,
            CreateIssueMilestoneRequest {
                repo: rref("acme/app"),
                title: "V1".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(dup), "ALREADY_EXISTS");
    let b = open(&w, &t.reader, "second").await;
    for n in [i.number, b.number] {
        s.set_issue_milestone(req(
            &t.writer,
            SetIssueMilestoneRequest {
                repo: rref("acme/app"),
                number: n,
                milestone_number: 1,
            },
        ))
        .await
        .unwrap();
    }
    let ms = || {
        s.get_issue_milestone(req(
            &t.reader,
            GetIssueMilestoneRequest {
                repo: rref("acme/app"),
                number: 1,
            },
        ))
    };
    let m1 = ms().await.unwrap().into_inner();
    assert_eq!((m1.open_items, m1.closed_items), (2, 0));
    close(&w, &t.writer, 1, IssueStateReason::Completed).await;
    let m2 = ms().await.unwrap().into_inner();
    assert_eq!((m2.open_items, m2.closed_items), (1, 1));
    let closed_ms = s
        .update_issue_milestone(req(
            &t.writer,
            UpdateIssueMilestoneRequest {
                repo: rref("acme/app"),
                number: 1,
                state: MilestoneState::Closed as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(closed_ms.closed_at.is_some());
    let open_only = s
        .list_issue_milestones(req(
            &t.reader,
            ListIssueMilestonesRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(open_only.milestones.is_empty());
    s.delete_issue_milestone(req(
        &t.writer,
        DeleteIssueMilestoneRequest {
            repo: rref("acme/app"),
            number: 1,
        },
    ))
    .await
    .unwrap();
    assert!(get(&w, &t.reader, 2).await.milestone.is_none());

    // Task lists count checkboxes outside code.
    let body = "- [x] a\n- [ ] b\n```\n- [ ] no\n```\n";
    s.update_issue(req(
        &t.reader,
        UpdateIssueRequest {
            repo: rref("acme/app"),
            number: 2,
            title: None,
            body: Some(body.into()),
            expected_version: 0,
        },
    ))
    .await
    .unwrap();
    let p = s
        .get_issue_task_list_progress(req(
            &t.reader,
            GetIssueTaskListProgressRequest {
                repo: rref("acme/app"),
                number: 2,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((p.total, p.completed), (2, 1));
    assert_eq!(get(&w, &t.reader, 2).await.task_list.unwrap().total, 2);
}

// ------------------------------------------------- comments, reactions, mentions

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn comments_edits_reactions_mentions_and_references() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let a = open(&w, &t.reader, "A").await;
    let b = open(&w, &t.reader, "B").await;
    let work = &w.env.forge.work;
    let sys = Cx::system();
    // A comment mentioning a user and referencing another issue.
    let c = s
        .create_issue_comment(req(
            &t.reader,
            CreateIssueCommentRequest {
                repo: rref("acme/app"),
                number: a.number,
                body: "ping @writer-login, see #2 and `#99`".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(c.author_association, "MEMBER");
    let ka = kinds(&timeline(&w, a.number).await);
    assert!(ka.contains(&IssueTimelineEventKind::Commented));
    assert!(ka.contains(&IssueTimelineEventKind::Mentioned));
    let kb = timeline(&w, b.number).await;
    let xref = kb
        .iter()
        .find(|e| e.kind == IssueTimelineEventKind::CrossReferenced as i32)
        .expect("cross reference on #2");
    assert_eq!(
        xref.source.as_ref().unwrap().kind,
        SubjectKind::Comment as i32
    );
    // Editing keeps history; no duplicate cross-reference.
    let (repo, _) = work.comment_location(&sys, &c.id).await.unwrap().unwrap();
    assert_eq!(repo, w.repo.id);
    let info = bee_forge::work::RepoInfo {
        id: w.repo.id.clone(),
        org_id: w.repo.org_id.clone(),
        full_name: w.repo.full_name.clone(),
    };
    let reader_actor = bee_forge::work::Actor {
        principal: Principal {
            id: "reader".into(),
            kind: PrincipalKind::Human as i32,
            display: "reader".into(),
            ..Default::default()
        },
        triage: false,
        admin: false,
        association: "MEMBER".into(),
    };
    let edited = work
        .update_comment(
            &sys,
            &info,
            &reader_actor,
            &c.id,
            "ping again, see #2",
            0,
            &Default::default(),
        )
        .await
        .unwrap();
    assert!(edited.edited);
    let (edits, _) = work.list_comment_edits(&sys, &c.id, 10, "").await.unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(
        edits[0].previous_body,
        "ping @writer-login, see #2 and `#99`"
    );
    let stranger = bee_forge::work::Actor {
        principal: Principal {
            id: "writer".into(),
            ..Default::default()
        },
        triage: false,
        ..Default::default()
    };
    let denied = work
        .update_comment(
            &sys,
            &info,
            &stranger,
            &c.id,
            "hijack",
            0,
            &Default::default(),
        )
        .await;
    assert_eq!(denied.unwrap_err().reason_name(), "PERMISSION_DENIED");
    // Reactions count per content and per user; adding twice is idempotent.
    let r1 = work
        .add_reaction(&sys, &info, &reader_actor, &c.id, ReactionContent::Rocket)
        .await
        .unwrap();
    let r2 = work
        .add_reaction(&sys, &info, &reader_actor, &c.id, ReactionContent::Rocket)
        .await
        .unwrap();
    assert_eq!(r1.id, r2.id);
    work.add_reaction(&sys, &info, &reader_actor, &a.id, ReactionContent::ThumbsUp)
        .await
        .unwrap();
    let (rs, groups, _) = work
        .list_reactions(&sys, &c.id, ReactionContent::Unspecified, 10, "")
        .await
        .unwrap();
    assert_eq!((rs.len(), groups.len()), (1, 1));
    let listed = s
        .list_issue_comments(req(
            &t.reader,
            ListIssueCommentsRequest {
                repo: rref("acme/app"),
                number: a.number,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.comments.len(), 1);
    assert_eq!(listed.comments[0].reactions[0].count, 1);
    assert_eq!(
        get(&w, &t.reader, 1).await.reactions[0].content,
        ReactionContent::ThumbsUp as i32
    );
    work.remove_reaction(&sys, &info, &reader_actor, &c.id, ReactionContent::Rocket)
        .await
        .unwrap();
    assert!(
        work.get_comment(&sys, &c.id)
            .await
            .unwrap()
            .reactions
            .is_empty()
    );
    // Deleting a comment drops it from the count and the timeline.
    work.delete_comment(&sys, &info, &reader_actor, &c.id)
        .await
        .unwrap();
    assert_eq!(get(&w, &t.reader, 1).await.comment_count, 0);
    assert!(!kinds(&timeline(&w, a.number).await).contains(&IssueTimelineEventKind::Commented));
}

// ------------------------------------------------------- types and fields

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_types_and_custom_fields() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let mk_type = |tok: &str, name: &str| {
        s.create_issue_type(req(
            tok,
            CreateIssueTypeRequest {
                org: "acme".into(),
                name: name.into(),
                color: "ff0000".into(),
                enabled: true,
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(mk_type(&t.writer, "Bug").await), "PERMISSION_DENIED");
    let bug = mk_type(&t.admin, "Bug").await.unwrap().into_inner();
    let feature = mk_type(&t.owner, "Feature").await.unwrap().into_inner();
    assert_eq!(reason(mk_type(&t.owner, "bug").await), "ALREADY_EXISTS");
    // Fields: kinds validate, applicability follows types.
    let prio = s
        .create_issue_field(req(
            &t.admin,
            CreateIssueFieldRequest {
                org: "acme".into(),
                name: "Priority".into(),
                kind: IssueFieldKind::SingleSelect as i32,
                options: ["P0", "P1", "P2"]
                    .iter()
                    .map(|n| IssueFieldOption {
                        name: n.to_string(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let effort = s
        .create_issue_field(req(
            &t.admin,
            CreateIssueFieldRequest {
                org: "acme".into(),
                name: "Effort".into(),
                kind: IssueFieldKind::Number as i32,
                issue_type_ids: vec![feature.id.clone()],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let target = s
        .create_issue_field(req(
            &t.admin,
            CreateIssueFieldRequest {
                org: "acme".into(),
                name: "Target date".into(),
                kind: IssueFieldKind::Date as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let notes = s
        .create_issue_field(req(
            &t.admin,
            CreateIssueFieldRequest {
                org: "acme".into(),
                name: "Notes".into(),
                kind: IssueFieldKind::Text as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let i = open(&w, &t.reader, "typed").await;
    let set_type = |ty: &str| {
        s.set_issue_type(req(
            &t.writer,
            SetIssueTypeRequest {
                repo: rref("acme/app"),
                number: i.number,
                issue_type: ty.into(),
            },
        ))
    };
    assert_eq!(
        set_type("Bug")
            .await
            .unwrap()
            .into_inner()
            .issue_type
            .unwrap()
            .name,
        "Bug"
    );
    let value = |field: &str, v: issue_field_value::Value| IssueFieldValue {
        field_id: field.into(),
        value: Some(v),
        ..Default::default()
    };
    let set_values = |vals: Vec<IssueFieldValue>| {
        s.set_issue_field_values(req(
            &t.writer,
            SetIssueFieldValuesRequest {
                repo: rref("acme/app"),
                number: i.number,
                values: vals,
            },
        ))
    };
    // Effort applies to features only.
    assert_eq!(
        reason(
            set_values(vec![value(
                &effort.id,
                issue_field_value::Value::Number(3.0)
            )])
            .await
        ),
        "INVALID_ARGUMENT"
    );
    // Wrong kind or unknown option.
    assert_eq!(
        reason(
            set_values(vec![value(
                &prio.id,
                issue_field_value::Value::Text("P0".into())
            )])
            .await
        ),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(
            set_values(vec![value(
                &prio.id,
                issue_field_value::Value::OptionId("P9".into())
            )])
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let ok = set_values(vec![
        value(&prio.id, issue_field_value::Value::OptionId("p1".into())),
        value(&notes.id, issue_field_value::Value::Text("careful".into())),
        value(
            &target.id,
            issue_field_value::Value::DateOn(pbjson_types::Timestamp {
                seconds: 1_700_000_000,
                nanos: 0,
            }),
        ),
    ])
    .await
    .unwrap()
    .into_inner();
    assert_eq!(ok.field_values.len(), 3);
    let p = ok
        .field_values
        .iter()
        .find(|v| v.field_name == "Priority")
        .unwrap();
    assert_eq!(p.kind, IssueFieldKind::SingleSelect as i32);
    assert_eq!(
        p.value,
        Some(issue_field_value::Value::OptionId(
            prio.options[1].id.clone()
        ))
    );
    let fc = timeline(&w, i.number).await;
    assert!(
        fc.iter()
            .any(|e| e.field_name == "Priority" && e.current_value == "P1")
    );
    // Type change drops values of fields that no longer apply; features take Effort.
    set_type("Feature").await.unwrap();
    let eff = set_values(vec![value(
        &effort.id,
        issue_field_value::Value::Number(5.0),
    )])
    .await
    .unwrap()
    .into_inner();
    assert_eq!(eff.field_values.len(), 4);
    set_type("Bug").await.unwrap();
    assert_eq!(get(&w, &t.reader, i.number).await.field_values.len(), 3);
    // Disabled types cannot be set; deleting a type untypes issues; deleting a field drops values.
    s.update_issue_type(req(
        &t.admin,
        UpdateIssueTypeRequest {
            org: "acme".into(),
            id: feature.id.clone(),
            enabled: Some(false),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(reason(set_type("Feature").await), "INVALID_ARGUMENT");
    let listed = s
        .list_issue_types(req(
            &t.reader,
            ListIssueTypesRequest {
                org: "acme".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.issue_types.len(), 1);
    s.delete_issue_type(req(
        &t.admin,
        DeleteIssueTypeRequest {
            org: "acme".into(),
            id: bug.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert!(get(&w, &t.reader, i.number).await.issue_type.is_none());
    s.delete_issue_field(req(
        &t.admin,
        DeleteIssueFieldRequest {
            org: "acme".into(),
            id: notes.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert!(
        get(&w, &t.reader, i.number)
            .await
            .field_values
            .iter()
            .all(|v| v.field_name != "Notes")
    );
    // Removing an option clears the values that used it.
    s.update_issue_field(req(
        &t.admin,
        UpdateIssueFieldRequest {
            org: "acme".into(),
            id: prio.id.clone(),
            options: vec![prio.options[0].clone(), prio.options[2].clone()],
            set_options: true,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert!(
        get(&w, &t.reader, i.number)
            .await
            .field_values
            .iter()
            .all(|v| v.field_name != "Priority")
    );
    let cleared = s
        .clear_issue_field_value(req(
            &t.writer,
            ClearIssueFieldValueRequest {
                repo: rref("acme/app"),
                number: i.number,
                field_id: "Target date".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(cleared.field_values.is_empty());
}

// --------------------------------------------- sub-issues and dependencies

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sub_issues_dependencies_cycles_and_progress() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let mut ids = Vec::new();
    for n in 1..=5 {
        ids.push(open(&w, &t.writer, &format!("issue {n}")).await);
    }
    let add_sub = |parent: u64, child: u64, replace: bool| {
        s.add_issue_sub_issue(req(
            &t.writer,
            AddIssueSubIssueRequest {
                repo: rref("acme/app"),
                number: parent,
                sub_issue: iref("acme/app", child),
                replace_parent: replace,
                after_id: String::new(),
            },
        ))
    };
    add_sub(1, 2, false).await.unwrap();
    add_sub(1, 3, false).await.unwrap();
    add_sub(2, 4, false).await.unwrap();
    // Cycles and second parents.
    assert_eq!(reason(add_sub(4, 1, false).await), "INVALID_ARGUMENT");
    assert_eq!(reason(add_sub(1, 1, false).await), "INVALID_ARGUMENT");
    assert_eq!(reason(add_sub(5, 3, false).await), "INVALID_STATE");
    let moved = add_sub(5, 3, true).await.unwrap().into_inner();
    assert_eq!(moved.sub_issues.unwrap().total, 1);
    assert_eq!(get(&w, &t.reader, 3).await.parent.unwrap().number, 5);
    assert_eq!(get(&w, &t.reader, 1).await.sub_issues.unwrap().total, 1);
    // Progress counts direct children and all descendants.
    close(&w, &t.writer, 4, IssueStateReason::Completed).await;
    let listed = s
        .list_issue_sub_issues(req(
            &t.reader,
            ListIssueSubIssuesRequest {
                repo: rref("acme/app"),
                number: 1,
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let p = listed.progress.unwrap();
    assert_eq!((p.total, p.completed, p.percent_completed), (1, 0, 50));
    assert_eq!(listed.sub_issues[0].number, 2);
    let parent = s
        .get_issue_parent(req(
            &t.reader,
            GetIssueParentRequest {
                repo: rref("acme/app"),
                number: 4,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(parent.parent.unwrap().number, 2);
    // Order.
    add_sub(1, 3, true).await.unwrap();
    let ordered = || {
        s.list_issue_sub_issues(req(
            &t.reader,
            ListIssueSubIssuesRequest {
                repo: rref("acme/app"),
                number: 1,
                page: None,
            },
        ))
    };
    let nums: Vec<u64> = ordered()
        .await
        .unwrap()
        .into_inner()
        .sub_issues
        .iter()
        .map(|i| i.number)
        .collect();
    assert_eq!(nums, vec![2, 3]);
    s.reorder_issue_sub_issue(req(
        &t.writer,
        ReorderIssueSubIssueRequest {
            repo: rref("acme/app"),
            number: 1,
            sub_issue: iref("acme/app", 3),
            after_id: String::new(),
            before_id: ids[1].id.clone(),
        },
    ))
    .await
    .unwrap();
    let nums: Vec<u64> = ordered()
        .await
        .unwrap()
        .into_inner()
        .sub_issues
        .iter()
        .map(|i| i.number)
        .collect();
    assert_eq!(nums, vec![3, 2]);
    // Removing detaches.
    s.remove_issue_sub_issue(req(
        &t.writer,
        RemoveIssueSubIssueRequest {
            repo: rref("acme/app"),
            number: 1,
            sub_issue: iref("acme/app", 3),
        },
    ))
    .await
    .unwrap();
    assert!(get(&w, &t.reader, 3).await.parent.is_none());
    // Depth limit: a chain of eight is fine, a ninth level is not.
    let mut chain = Vec::new();
    for n in 0..9 {
        chain.push(open(&w, &t.writer, &format!("chain {n}")).await.number);
    }
    for pair in chain.windows(2).take(7) {
        add_sub(pair[0], pair[1], false).await.unwrap();
    }
    assert_eq!(
        reason(add_sub(chain[7], chain[8], false).await),
        "RESOURCE_EXHAUSTED"
    );

    // Dependencies.
    let dep = |n: u64, by: u64| {
        s.add_issue_dependency(req(
            &t.writer,
            AddIssueDependencyRequest {
                repo: rref("acme/app"),
                number: n,
                blocked_by: iref("acme/app", by),
            },
        ))
    };
    let d = dep(1, 2).await.unwrap().into_inner();
    let ds = d.dependencies.unwrap();
    assert_eq!((ds.blocked_by, ds.blocked_by_open, ds.blocking), (1, 1, 0));
    assert_eq!(reason(dep(1, 2).await), "ALREADY_EXISTS");
    assert_eq!(reason(dep(2, 1).await), "INVALID_ARGUMENT");
    dep(2, 5).await.unwrap();
    assert_eq!(reason(dep(5, 1).await), "INVALID_ARGUMENT");
    close(&w, &t.writer, 2, IssueStateReason::Completed).await;
    assert_eq!(
        get(&w, &t.reader, 1)
            .await
            .dependencies
            .unwrap()
            .blocked_by_open,
        0
    );
    let blocking = s
        .list_issue_dependencies(req(
            &t.reader,
            ListIssueDependenciesRequest {
                repo: rref("acme/app"),
                number: 2,
                direction: IssueDependencyDirection::Blocking as i32,
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(blocking.issues[0].number, 1);
    assert_eq!(
        reason(
            s.list_issue_dependencies(req(
                &t.reader,
                ListIssueDependenciesRequest {
                    repo: rref("acme/app"),
                    number: 2,
                    direction: 0,
                    page: None
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    s.remove_issue_dependency(req(
        &t.writer,
        RemoveIssueDependencyRequest {
            repo: rref("acme/app"),
            number: 1,
            blocked_by: iref("acme/app", 2),
        },
    ))
    .await
    .unwrap();
    assert!(get(&w, &t.reader, 1).await.dependencies.is_none());
    // Deleting an issue cleans its relations.
    s.delete_issue(req(
        &t.admin,
        DeleteIssueRequest {
            repo: rref("acme/app"),
            number: 5,
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        get(&w, &t.reader, 2)
            .await
            .dependencies
            .map(|d| d.blocked_by)
            .unwrap_or(0),
        0
    );
}

// -------------------------------------------------------------- transfer

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_moves_the_issue_and_keeps_a_redirect() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let i = open(&w, &t.writer, "moving").await;
    open(&w, &t.writer, "stays").await;
    s.add_issue_labels(req(
        &t.writer,
        AddIssueLabelsRequest {
            repo: rref("acme/app"),
            number: 1,
            labels: vec!["bug".into(), "wontfix".into()],
            create_missing: false,
        },
    ))
    .await
    .unwrap();
    s.delete_issue_label(req(
        &t.writer,
        DeleteIssueLabelRequest {
            repo: rref("acme/tools"),
            name: "wontfix".into(),
        },
    ))
    .await
    .unwrap();
    s.create_issue_milestone(req(
        &t.writer,
        CreateIssueMilestoneRequest {
            repo: rref("acme/app"),
            title: "only here".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    s.set_issue_milestone(req(
        &t.writer,
        SetIssueMilestoneRequest {
            repo: rref("acme/app"),
            number: 1,
            milestone_number: 1,
        },
    ))
    .await
    .unwrap();
    // Reader cannot, a stranger repo name is not found, same repo is invalid.
    let xfer = |tok: &str, to: &str| {
        s.transfer_issue(req(
            tok,
            TransferIssueRequest {
                repo: rref("acme/app"),
                number: 1,
                target_repo: rref(to),
                create_labels: false,
            },
        ))
    };
    assert_eq!(
        reason(xfer(&t.reader, "acme/tools").await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(xfer(&t.writer, "acme/app").await),
        "INVALID_ARGUMENT"
    );
    let moved = xfer(&t.writer, "acme/tools").await.unwrap().into_inner();
    let issue = moved.issue.unwrap();
    assert_eq!(issue.repo_full_name, "acme/tools");
    assert_eq!(issue.number, 1);
    assert_eq!(issue.id, i.id);
    assert_eq!(issue.transferred_from.unwrap().number, 1);
    assert_eq!(
        issue
            .labels
            .iter()
            .map(|l| l.name.clone())
            .collect::<Vec<_>>(),
        vec!["bug"]
    );
    assert!(issue.milestone.is_none());
    assert_eq!(moved.redirect_from.unwrap().number, 1);
    // The old number redirects; the next issue in the old repo continues the sequence.
    let via_old = get(&w, &t.reader, 1).await;
    assert_eq!(via_old.repo_full_name, "acme/tools");
    assert_eq!(via_old.id, i.id);
    assert_eq!(open(&w, &t.writer, "after").await.number, 3);
    let (comments_ok, ks) = (
        timeline(&w, 2).await,
        kinds(
            &s.list_issue_timeline(req(
                &t.owner,
                ListIssueTimelineRequest {
                    repo: rref("acme/tools"),
                    number: 1,
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .events,
        ),
    );
    assert!(comments_ok.is_empty() || !kinds(&comments_ok).is_empty());
    assert!(ks.contains(&IssueTimelineEventKind::Transferred));
    // Events reach both repos' logs.
    w.env.forge.flush().await.unwrap();
    let tools = s
        .get_repo(req(
            &t.owner,
            GetRepoRequest {
                repo: rref("acme/tools"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let (ev, _) = w
        .env
        .forge
        .events
        .read(&Cx::system(), &repo_log(&tools.id), 0, &[], 100)
        .await
        .unwrap();
    assert!(ev.iter().any(|e| e.kind == "issue.transferred"));
    // A reader of the source who cannot read the target gets NOT_FOUND through the redirect.
    s.update_repo(req(
        &t.owner,
        UpdateRepoRequest {
            repo: rref("acme/tools"),
            visibility: Visibility::Private as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    s.remove_collaborator(req(
        &t.owner,
        RemoveCollaboratorRequest {
            repo: rref("acme/tools"),
            principal_id: "writer".into(),
        },
    ))
    .await
    .ok();
}

// ------------------------------------------------------------- templates

struct Templates;

#[async_trait]
impl TemplateSource for Templates {
    async fn read_dir(&self, _repo: &Repo, dir: &str) -> FResult<Option<TemplateFiles>> {
        if dir != ".beehive/ISSUE_TEMPLATE" {
            return Ok(None);
        }
        let f = |name: &str, body: &str| TemplateFile {
            path: format!("{dir}/{name}"),
            content: body.as_bytes().to_vec(),
        };
        Ok(Some(TemplateFiles {
            revision: "rev-1".into(),
            files: vec![
                f(
                    "bug.yml",
                    "name: Bug report\ndescription: File a bug\ntitle: \"[Bug]: \"\nlabels: [bug, not-a-label]\ntype: Bug\nbody:\n  - type: input\n    id: version\n    attributes:\n      label: Version\n    validations:\n      required: true\n  - type: textarea\n    id: logs\n    attributes:\n      label: Logs\n      render: shell\n",
                ),
                f(
                    "idea.md",
                    "---\nname: Idea\nabout: Suggest something\nlabels: enhancement\n---\n## The idea\n",
                ),
                f("broken.yml", "name: Broken\ndescription: d\nbody: []\n"),
                f(
                    "config.yml",
                    "blank_issues_enabled: false\ncontact_links:\n  - name: Chat\n    url: https://example.com/chat\n    about: Ask there\n",
                ),
            ],
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn templates_forms_and_chooser_config() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    w.env.forge.work.set_hooks(WorkHooks {
        templates: Arc::new(Templates),
        ..WorkHooks::default()
    });
    s.create_issue_type(req(
        &t.admin,
        CreateIssueTypeRequest {
            org: "acme".into(),
            name: "Bug".into(),
            enabled: true,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let list = s
        .list_issue_templates(req(
            &t.reader,
            ListIssueTemplatesRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.revision, "rev-1");
    let names: Vec<_> = list.templates.iter().map(|t| t.file_name.clone()).collect();
    assert_eq!(names, vec!["broken.yml", "bug.yml", "idea.md"]);
    assert!(!list.templates[0].parse_error.is_empty());
    assert!(list.templates[1].is_form);
    assert_eq!(list.templates[1].form_fields.len(), 2);
    let cfg = s
        .get_issue_template_config(req(
            &t.reader,
            GetIssueTemplateConfigRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!cfg.blank_issues_enabled);
    assert_eq!(cfg.contact_links[0].name, "Chat");
    assert_eq!(cfg.revision, "rev-1");

    let file = |template: &str, answers: &[(&str, &str)]| CreateIssueRequest {
        repo: rref("acme/app"),
        title: String::new(),
        template: template.into(),
        form_responses: answers
            .iter()
            .map(|(k, v)| KeyValue {
                key: k.to_string(),
                value: v.to_string(),
            })
            .collect(),
        ..Default::default()
    };
    // A reader files from a form: defaults apply (known labels only), answers become the body.
    let i = s
        .create_issue(req(
            &t.reader,
            file("bug.yml", &[("version", "1.2.3"), ("logs", "boom")]),
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(i.title, "[Bug]:");
    assert!(i.body.contains("### Version\n\n1.2.3"));
    assert!(i.body.contains("```shell\nboom\n```"));
    assert_eq!(
        i.labels.iter().map(|l| l.name.clone()).collect::<Vec<_>>(),
        vec!["bug"]
    );
    assert_eq!(i.issue_type.unwrap().name, "Bug");
    // Required answers, unknown and broken templates.
    assert_eq!(
        reason(s.create_issue(req(&t.reader, file("bug.yml", &[]))).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(
            s.create_issue(req(&t.reader, file("broken.yml", &[])))
                .await
        ),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(s.create_issue(req(&t.reader, file("nope.yml", &[]))).await),
        "NOT_FOUND"
    );
    let idea = s.create_issue(req(&t.reader, file("idea.md", &[]))).await;
    assert!(idea.is_err()); // no title and the template has none
}

// ------------------------------------------------------- saved replies

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saved_replies_are_per_user() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let r = s
        .create_saved_reply(req(
            &t.writer,
            CreateSavedReplyRequest {
                title: "Thanks".into(),
                body: "Thanks for the report!".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        reason(
            s.create_saved_reply(req(
                &t.writer,
                CreateSavedReplyRequest {
                    title: "thanks".into(),
                    body: "x".into()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    // Another user neither sees nor edits it.
    assert_eq!(
        reason(
            s.get_saved_reply(req(&t.reader, GetSavedReplyRequest { id: r.id.clone() }))
                .await
        ),
        "NOT_FOUND"
    );
    assert!(
        s.list_saved_replies(req(&t.reader, ListSavedRepliesRequest::default()))
            .await
            .unwrap()
            .into_inner()
            .saved_replies
            .is_empty()
    );
    let u = s
        .update_saved_reply(req(
            &t.writer,
            UpdateSavedReplyRequest {
                id: r.id.clone(),
                title: None,
                body: Some("Thanks!".into()),
                expected_version: 1,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(u.version, 2);
    assert_eq!(
        reason(
            s.update_saved_reply(req(
                &t.writer,
                UpdateSavedReplyRequest {
                    id: r.id.clone(),
                    title: None,
                    body: Some("x".into()),
                    expected_version: 1
                }
            ))
            .await
        ),
        "VERSION_MISMATCH"
    );
    // Using one in a comment is recorded; an unknown id is rejected.
    let i = open(&w, &t.reader, "q").await;
    let used = s
        .create_issue_comment(req(
            &t.writer,
            CreateIssueCommentRequest {
                repo: rref("acme/app"),
                number: i.number,
                body: u.body.clone(),
                saved_reply_id: r.id.clone(),
                ..Default::default()
            },
        ))
        .await;
    assert!(used.is_ok());
    let bad = s
        .create_issue_comment(req(
            &t.writer,
            CreateIssueCommentRequest {
                repo: rref("acme/app"),
                number: i.number,
                body: "x".into(),
                saved_reply_id: "nope".into(),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(bad), "NOT_FOUND");
    s.delete_saved_reply(req(&t.writer, DeleteSavedReplyRequest { id: r.id }))
        .await
        .unwrap();
    assert!(
        s.list_saved_replies(req(&t.writer, ListSavedRepliesRequest::default()))
            .await
            .unwrap()
            .into_inner()
            .saved_replies
            .is_empty()
    );
}

// ---------------------------------------------------------------- search

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_filters_and_issue_search_syntax() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let a = open(&w, &t.writer, "Crash on login").await; // 1
    open(&w, &t.reader, "Docs typo").await; // 2
    open(&w, &t.writer, "Login page slow").await; // 3
    let d = open(&w, &t.reader, "Dependency bump").await; // 4
    s.add_issue_labels(req(
        &t.writer,
        AddIssueLabelsRequest {
            repo: rref("acme/app"),
            number: 1,
            labels: vec!["bug".into()],
            create_missing: false,
        },
    ))
    .await
    .unwrap();
    s.add_issue_labels(req(
        &t.writer,
        AddIssueLabelsRequest {
            repo: rref("acme/app"),
            number: 3,
            labels: vec!["bug".into(), "help wanted".into()],
            create_missing: false,
        },
    ))
    .await
    .unwrap();
    s.add_issue_assignees(req(
        &t.writer,
        AddIssueAssigneesRequest {
            repo: rref("acme/app"),
            number: 1,
            assignees: vec!["writer".into()],
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    s.create_issue_milestone(req(
        &t.writer,
        CreateIssueMilestoneRequest {
            repo: rref("acme/app"),
            title: "v2".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    s.set_issue_milestone(req(
        &t.writer,
        SetIssueMilestoneRequest {
            repo: rref("acme/app"),
            number: 3,
            milestone_number: 1,
        },
    ))
    .await
    .unwrap();
    close(&w, &t.writer, 4, IssueStateReason::NotPlanned).await;
    s.create_issue_comment(req(
        &t.reader,
        CreateIssueCommentRequest {
            repo: rref("acme/app"),
            number: 2,
            body: "ping @writer-login about flaky".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let _ = (a, d);
    assert_eq!(list(&w, &t.reader, "").await, vec![1, 2, 3, 4]);
    assert_eq!(list(&w, &t.reader, "is:open").await, vec![1, 2, 3]);
    assert_eq!(list(&w, &t.reader, "is:closed").await, vec![4]);
    assert_eq!(
        list(&w, &t.reader, "state:closed reason:not-planned").await,
        vec![4]
    );
    assert_eq!(
        list(&w, &t.reader, "reason:completed").await,
        Vec::<u64>::new()
    );
    assert_eq!(list(&w, &t.reader, "label:bug").await, vec![1, 3]);
    assert_eq!(
        list(&w, &t.reader, "label:bug label:\"help wanted\"").await,
        vec![3]
    );
    assert_eq!(
        list(&w, &t.reader, "label:bug -label:\"help wanted\"").await,
        vec![1]
    );
    assert_eq!(list(&w, &t.reader, "no:label").await, vec![2, 4]);
    assert_eq!(list(&w, &t.reader, "assignee:writer-login").await, vec![1]);
    assert_eq!(list(&w, &t.reader, "no:assignee is:open").await, vec![2, 3]);
    assert_eq!(list(&w, &t.reader, "author:reader-login").await, vec![2, 4]);
    assert_eq!(list(&w, &t.writer, "author:@me").await, vec![1, 3]);
    assert_eq!(list(&w, &t.reader, "milestone:v2").await, vec![3]);
    assert_eq!(list(&w, &t.reader, "no:milestone").await, vec![1, 2, 4]);
    assert_eq!(list(&w, &t.reader, "login").await, vec![1, 3]);
    assert_eq!(list(&w, &t.reader, "login in:title -slow").await, vec![1]);
    assert_eq!(list(&w, &t.reader, "flaky in:comments").await, vec![2]);
    assert_eq!(list(&w, &t.reader, "flaky").await, Vec::<u64>::new());
    assert_eq!(list(&w, &t.reader, "mentions:writer-login").await, vec![2]);
    assert_eq!(list(&w, &t.reader, "commenter:reader-login").await, vec![2]);
    assert_eq!(
        list(&w, &t.reader, "involves:writer-login").await,
        vec![1, 2, 3]
    );
    assert_eq!(list(&w, &t.reader, "comments:>=1").await, vec![2]);
    assert_eq!(list(&w, &t.reader, "comments:0").await, vec![1, 3, 4]);
    assert_eq!(
        list(&w, &t.reader, "is:issue is:open label:bug").await,
        vec![1, 3]
    );
    assert_eq!(list(&w, &t.reader, "is:pr").await, Vec::<u64>::new());
    assert_eq!(
        list(&w, &t.reader, "created:>=2000-01-01").await,
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        list(&w, &t.reader, "created:<2000-01-01").await,
        Vec::<u64>::new()
    );
    assert_eq!(
        list(&w, &t.reader, "updated:@today").await,
        vec![1, 2, 3, 4]
    );
    assert_eq!(list(&w, &t.reader, "is:locked").await, Vec::<u64>::new());
    assert_eq!(list(&w, &t.reader, "agent:false").await, vec![1, 2, 3, 4]);
    // Bad queries are INVALID_ARGUMENT with the parser's subcode.
    let bad = s
        .list_issues(req(
            &t.reader,
            ListIssuesRequest {
                repo: rref("acme/app"),
                query: "nonsense:thing".into(),
                ..Default::default()
            },
        ))
        .await;
    let st = bad.unwrap_err();
    let d = bee_proto::errors::detail_from_status(&st).unwrap();
    assert_eq!(
        (d.reason.as_str(), d.subcode.as_str()),
        ("INVALID_ARGUMENT", "unknown_qualifier")
    );
    // Request-level filters, sorting and paging.
    let r = s
        .list_issues(req(
            &t.reader,
            ListIssuesRequest {
                repo: rref("acme/app"),
                labels: vec!["bug".into()],
                assignee: "none".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        r.issues.iter().map(|i| i.number).collect::<Vec<_>>(),
        vec![3]
    );
    let mut seen = Vec::new();
    let mut token = String::new();
    loop {
        let p = s
            .list_issues(req(
                &t.reader,
                ListIssuesRequest {
                    repo: rref("acme/app"),
                    all_states: true,
                    sort: IssueSort::Created as i32,
                    ascending: true,
                    page: Some(PageRequest {
                        page_size: 3,
                        page_token: token.clone(),
                    }),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        seen.extend(p.issues.iter().map(|i| i.number));
        token = p.page.map(|p| p.next_page_token).unwrap_or_default();
        if token.is_empty() {
            break;
        }
    }
    assert_eq!(seen, vec![1, 2, 3, 4]);
    let by_comments = s
        .list_issues(req(
            &t.reader,
            ListIssuesRequest {
                repo: rref("acme/app"),
                all_states: true,
                sort: IssueSort::Comments as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_comments.issues[0].number, 2);
    let query_sort = s
        .list_issues(req(
            &t.reader,
            ListIssuesRequest {
                repo: rref("acme/app"),
                all_states: true,
                query: "sort:created-asc".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(query_sort.issues[0].number, 1);
}

// ------------------------------------------------- agents (AS-ISSUE-02)

struct NoBudget;

#[async_trait]
impl AgentGate for NoBudget {
    async fn admit(&self, _repo: &Repo, _agent: &Principal) -> FResult<bool> {
        Err(bee_forge::ForgeError::new(
            ErrorReason::BudgetExhausted,
            "no budget left",
        ))
    }
}

fn change_event(w: &W, kind: &str, id: &str, number: u64, body: &str, author: Principal) -> Event {
    Event {
        kind: kind.into(),
        repo_id: w.repo.id.clone(),
        org_id: w.repo.org_id.clone(),
        body: Some(event::Body::Change(ChangeRequest {
            id: id.into(),
            repo_id: w.repo.id.clone(),
            number,
            title: "Fix the crash".into(),
            body: body.into(),
            target_ref: w.repo.default_ref.clone(),
            author: Some(author),
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn agent_principal() -> Principal {
    Principal {
        id: "agent".into(),
        kind: PrincipalKind::Agent as i32,
        display: "agent".into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_issue_02_assigned_agent_implements_and_closes_through_a_landed_cr() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let forge = &w.env.forge;
    let issue = open(&w, &t.reader, "Crash on login").await;
    // Assigning an agent starts a run and tells the runner.
    let res = s
        .add_issue_assignees(req(
            &t.writer,
            AddIssueAssigneesRequest {
                repo: rref("acme/app"),
                number: issue.number,
                assignees: vec!["agent".into()],
                agent_instructions: "keep the diff small".into(),
                base_ref: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(res.agent_runs.len(), 1);
    let run_id = res.agent_runs[0].agent_run_id.clone();
    let assigned = res.issue.unwrap();
    assert_eq!(assigned.active_agent_run_ids, vec![run_id.clone()]);
    assert_eq!(assigned.assignees[0].kind, PrincipalKind::Agent as i32);
    forge.flush().await.unwrap();
    let (events, _) = forge
        .events
        .read(&Cx::system(), &repo_log(&w.repo.id), 0, &[], 1000)
        .await
        .unwrap();
    let ev = events
        .iter()
        .find(|e| e.kind == "issue.assigned_to_agent")
        .expect("runner event");
    let payload: serde_json::Value = serde_json::from_str(&ev.payload_json).unwrap();
    assert_eq!(payload["agent_run_id"], run_id.as_str());
    assert_eq!(payload["instructions"], "keep the diff small");
    assert_eq!(payload["number"], 1);
    assert_eq!(payload["agent"]["id"], "agent");
    assert_eq!(payload["base_ref"], w.repo.default_ref.as_str());
    let kinds_now = kinds(&timeline(&w, 1).await);
    assert!(kinds_now.contains(&IssueTimelineEventKind::Assigned));
    assert!(kinds_now.contains(&IssueTimelineEventKind::AgentRunStarted));
    // Assigning the same agent again starts nothing.
    let again = s
        .add_issue_assignees(req(
            &t.writer,
            AddIssueAssigneesRequest {
                repo: rref("acme/app"),
                number: 1,
                assignees: vec!["agent".into()],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(again.agent_runs.is_empty());

    // The agent opens a change request that says it fixes the issue.
    let cr_number = forge
        .work
        .allocate_number(
            &Cx::system(),
            &w.repo.id,
            NumberKind::ChangeRequest,
            "CR-AGENT",
        )
        .await
        .unwrap();
    assert_eq!(cr_number, 2);
    forge
        .events
        .append(
            &Cx::system(),
            change_event(
                &w,
                "cr.opened",
                "CR-AGENT",
                cr_number,
                "Fixes #1",
                agent_principal(),
            ),
            "cr-opened",
        )
        .await
        .unwrap();
    let dev = s
        .get_issue_development(req(
            &t.reader,
            GetIssueDevelopmentRequest {
                repo: rref("acme/app"),
                number: 1,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dev.changes.len(), 1);
    assert_eq!(dev.changes[0].source, IssueLinkSource::AgentRun as i32);
    assert_eq!(dev.changes[0].repo_full_name, "acme/app");
    assert!(dev.changes[0].closes);
    assert_eq!(dev.changes[0].state, "open");
    assert_eq!(
        get(&w, &t.reader, 1).await.linked_change_request_ids,
        vec!["CR-AGENT".to_string()]
    );
    // The link an agent run made stays when the body is edited (only keyword links follow the text).
    forge
        .events
        .append(
            &Cx::system(),
            change_event(
                &w,
                "cr.edited",
                "CR-AGENT",
                cr_number,
                "no keyword",
                agent_principal(),
            ),
            "e1",
        )
        .await
        .unwrap();
    assert_eq!(
        get(&w, &t.reader, 1).await.linked_change_request_ids.len(),
        1
    );
    // Landing closes the issue, finishes the run and shows in the timeline.
    forge
        .events
        .append(
            &Cx::system(),
            change_event(
                &w,
                "cr.landed",
                "CR-AGENT",
                cr_number,
                "Closes #1",
                agent_principal(),
            ),
            "landed",
        )
        .await
        .unwrap();
    let done = get(&w, &t.reader, 1).await;
    assert_eq!(done.state, IssueState::Closed as i32);
    assert_eq!(done.state_reason, IssueStateReason::Completed as i32);
    assert!(done.active_agent_run_ids.is_empty());
    assert_eq!(done.closed_by.unwrap().id, "agent");
    let tl = timeline(&w, 1).await;
    let closed = tl
        .iter()
        .find(|e| e.kind == IssueTimelineEventKind::Closed as i32)
        .unwrap();
    assert_eq!(closed.change_request_id, "CR-AGENT");
    assert!(kinds(&tl).contains(&IssueTimelineEventKind::AgentRunFinished));
    let dev = s
        .get_issue_development(req(
            &t.reader,
            GetIssueDevelopmentRequest {
                repo: rref("acme/app"),
                number: 1,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dev.changes[0].state, "landed");
    // Events carry the whole story to subscribers.
    let all = event_kinds(&w).await;
    for k in [
        "issue.opened",
        "issue.assigned",
        "issue.assigned_to_agent",
        "issue.change_linked",
        "issue.closed",
        "issue.agent_run_finished",
    ] {
        assert!(all.iter().any(|x| x == k), "missing {k}: {all:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_assignment_rules() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    open(&w, &t.reader, "work").await;
    // The agent token itself may open issues and comment as itself.
    let by_agent = s
        .create_issue(req(
            &t.agent,
            CreateIssueRequest {
                repo: rref("acme/app"),
                title: "found a bug".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_agent.author.unwrap().kind, PrincipalKind::Agent as i32);
    // Not assignable: an outsider; unknown principal.
    for who in ["outsider", "nobody"] {
        let r = s
            .add_issue_assignees(req(
                &t.writer,
                AddIssueAssigneesRequest {
                    repo: rref("acme/app"),
                    number: 1,
                    assignees: vec![who.into()],
                    ..Default::default()
                },
            ))
            .await;
        assert_eq!(reason(r), "INVALID_ARGUMENT", "{who}");
    }
    // A funding failure aborts before anything changes.
    w.env.forge.work.set_hooks(WorkHooks {
        agents: Arc::new(NoBudget),
        ..WorkHooks::default()
    });
    let r = s
        .add_issue_assignees(req(
            &t.writer,
            AddIssueAssigneesRequest {
                repo: rref("acme/app"),
                number: 1,
                assignees: vec!["agent".into()],
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(r), "BUDGET_EXHAUSTED");
    assert!(get(&w, &t.reader, 1).await.assignees.is_empty());
    // Humans do not need the gate.
    w.env.forge.work.set_hooks(WorkHooks::default());
    let r = s
        .add_issue_assignees(req(
            &t.writer,
            AddIssueAssigneesRequest {
                repo: rref("acme/app"),
                number: 1,
                assignees: vec!["writer".into(), "agent".into()],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.agent_runs.len(), 1);
    // Unassigning keeps the run unless asked to stop it.
    let kept = s
        .remove_issue_assignees(req(
            &t.writer,
            RemoveIssueAssigneesRequest {
                repo: rref("acme/app"),
                number: 1,
                assignees: vec!["agent".into()],
                stop_agent_runs: false,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(kept.active_agent_run_ids.len(), 1);
    s.add_issue_assignees(req(
        &t.writer,
        AddIssueAssigneesRequest {
            repo: rref("acme/app"),
            number: 1,
            assignees: vec!["agent".into()],
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let stopped = s
        .remove_issue_assignees(req(
            &t.writer,
            RemoveIssueAssigneesRequest {
                repo: rref("acme/app"),
                number: 1,
                assignees: vec!["agent".into()],
                stop_agent_runs: true,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(stopped.active_agent_run_ids.is_empty());
    assert_eq!(stopped.assignees.len(), 1);
    // The assignable list includes humans and agents and filters by kind.
    let all = s
        .list_issue_assignable_users(req(
            &t.reader,
            ListIssueAssignableUsersRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .principals;
    assert!(
        all.iter()
            .any(|p| p.id == "agent" && p.kind == PrincipalKind::Agent as i32)
    );
    assert!(all.iter().any(|p| p.id == "writer"));
    assert!(!all.iter().any(|p| p.id == "outsider"));
    let agents = s
        .list_issue_assignable_users(req(
            &t.reader,
            ListIssueAssignableUsersRequest {
                repo: rref("acme/app"),
                kind: PrincipalKind::Agent as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .principals;
    assert!(agents.iter().all(|p| p.kind == PrincipalKind::Agent as i32));
    // Assignee cap.
    let cap = s
        .add_issue_assignees(req(
            &t.writer,
            AddIssueAssigneesRequest {
                repo: rref("acme/app"),
                number: 2,
                assignees: ["writer", "reader", "agent", "admin", "owner"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                ..Default::default()
            },
        ))
        .await;
    assert!(cap.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn development_links_and_branches() {
    struct Branches;
    #[async_trait]
    impl bee_forge::work::BranchProvisioner for Branches {
        async fn create_branch(
            &self,
            _repo: &Repo,
            name: &str,
            _base: &str,
            workspace: bool,
        ) -> FResult<String> {
            if name == "taken" {
                return Err(bee_forge::ForgeError::already_exists("ref taken"));
            }
            Ok(if workspace {
                "ws-1".into()
            } else {
                String::new()
            })
        }
    }
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let issue = open(&w, &t.writer, "Fix the login bug!").await;
    // Without a VCS attached branches are served elsewhere.
    let no_vcs = s
        .create_issue_branch(req(
            &t.writer,
            CreateIssueBranchRequest {
                repo: rref("acme/app"),
                number: 1,
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(no_vcs), "UNIMPLEMENTED");
    w.env.forge.work.set_hooks(WorkHooks {
        branches: Arc::new(Branches),
        ..WorkHooks::default()
    });
    let b = s
        .create_issue_branch(req(
            &t.writer,
            CreateIssueBranchRequest {
                repo: rref("acme/app"),
                number: 1,
                create_workspace: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(b.ref_name, "issue-1-fix-the-login-bug");
    assert_eq!(b.workspace_id, "ws-1");
    assert_eq!(b.repo_full_name, "acme/app");
    assert_eq!(
        reason(
            s.create_issue_branch(req(
                &t.writer,
                CreateIssueBranchRequest {
                    repo: rref("acme/app"),
                    number: 1,
                    ref_name: "taken".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    assert_eq!(
        reason(
            s.create_issue_branch(req(
                &t.reader,
                CreateIssueBranchRequest {
                    repo: rref("acme/app"),
                    number: 1,
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    // Manual links to a change request that holds a number in this repo.
    let n = w
        .env
        .forge
        .work
        .allocate_number(&Cx::system(), &w.repo.id, NumberKind::ChangeRequest, "CR-1")
        .await
        .unwrap();
    assert_eq!(n, 2);
    let link = s
        .link_issue_change(req(
            &t.writer,
            LinkIssueChangeRequest {
                repo: rref("acme/app"),
                number: 1,
                change_request_id: "CR-1".into(),
                closes: false,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (link.number, link.source),
        (2, IssueLinkSource::Manual as i32)
    );
    assert_eq!(
        reason(
            s.link_issue_change(req(
                &t.writer,
                LinkIssueChangeRequest {
                    repo: rref("acme/app"),
                    number: 1,
                    change_request_id: "CR-UNKNOWN".into(),
                    closes: false
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // A non-closing link does not close on landing.
    let forge = &w.env.forge;
    forge
        .events
        .append(
            &Cx::system(),
            change_event(&w, "cr.landed", "CR-1", 2, "", agent_principal()),
            "l",
        )
        .await
        .unwrap();
    assert_eq!(get(&w, &t.reader, 1).await.state, IssueState::Open as i32);
    let dev = s
        .get_issue_development(req(
            &t.reader,
            GetIssueDevelopmentRequest {
                repo: rref("acme/app"),
                number: 1,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dev.changes[0].state, "landed");
    assert_eq!(dev.branches.len(), 1);
    s.unlink_issue_change(req(
        &t.writer,
        UnlinkIssueChangeRequest {
            repo: rref("acme/app"),
            number: 1,
            change_request_id: "CR-1".into(),
        },
    ))
    .await
    .unwrap();
    assert!(
        get(&w, &t.reader, issue.number)
            .await
            .linked_change_request_ids
            .is_empty()
    );
    // Landing somewhere other than the default branch never closes.
    s.link_issue_change(req(
        &t.writer,
        LinkIssueChangeRequest {
            repo: rref("acme/app"),
            number: 1,
            change_request_id: "CR-1".into(),
            closes: true,
        },
    ))
    .await
    .unwrap();
    let mut other_branch = change_event(&w, "cr.landed", "CR-1", 2, "", agent_principal());
    if let Some(event::Body::Change(c)) = other_branch.body.as_mut() {
        c.target_ref = "release/1".into();
    }
    forge
        .events
        .append(&Cx::system(), other_branch, "l2")
        .await
        .unwrap();
    assert_eq!(get(&w, &t.reader, 1).await.state, IssueState::Open as i32);
    forge
        .events
        .append(
            &Cx::system(),
            change_event(&w, "cr.landed", "CR-1", 2, "", agent_principal()),
            "l3",
        )
        .await
        .unwrap();
    assert_eq!(get(&w, &t.reader, 1).await.state, IssueState::Closed as i32);
    // Replaying the event is harmless.
    forge
        .events
        .append(
            &Cx::system(),
            change_event(&w, "cr.landed", "CR-1", 2, "", agent_principal()),
            "l4",
        )
        .await
        .unwrap();
    assert_eq!(get(&w, &t.reader, 1).await.state, IssueState::Closed as i32);
}

// ------------------------------------------------------- import (SPEC 17.3)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn imported_issues_keep_their_numbers_and_history() {
    use bee_forge::work::{ImportedComment, ImportedIssue, RepoInfo};
    let w = world().await;
    let work = &w.env.forge.work;
    let sys = Cx::system();
    let info = RepoInfo {
        id: w.repo.id.clone(),
        org_id: w.repo.org_id.clone(),
        full_name: w.repo.full_name.clone(),
    };
    let human = |id: &str| Principal {
        id: id.into(),
        kind: PrincipalKind::Human as i32,
        display: id.into(),
        ..Default::default()
    };
    let imp = |number: u64, closed: Option<u64>| ImportedIssue {
        number,
        title: format!("imported {number}"),
        body: "from github".into(),
        author: human("octocat"),
        created_ms: 1_600_000_000_000,
        closed_ms: closed,
        state_reason: IssueStateReason::NotPlanned,
        labels: vec![("legacy".into(), "abcdef".into())],
        milestone: "old".into(),
        comments: vec![ImportedComment {
            author: human("octocat"),
            body: "first!".into(),
            created_ms: 1_600_000_100_000,
        }],
        ..Default::default()
    };
    let a = work.import_issue(&sys, &info, imp(42, None)).await.unwrap();
    assert_eq!((a.number, a.comment_count), (42, 1));
    assert_eq!(a.created_at.unwrap().seconds, 1_600_000_000);
    let b = work
        .import_issue(&sys, &info, imp(7, Some(1_650_000_000_000)))
        .await
        .unwrap();
    assert_eq!(b.state, IssueState::Closed as i32);
    assert_eq!(b.state_reason, IssueStateReason::NotPlanned as i32);
    assert_eq!(b.closed_at.unwrap().seconds, 1_650_000_000);
    assert_eq!(b.labels[0].name, "legacy");
    assert_eq!(b.milestone.unwrap().closed_items, 1);
    // The number space continues after the highest imported number; used numbers are refused.
    assert!(work.import_issue(&sys, &info, imp(42, None)).await.is_err());
    assert_eq!(open(&w, &w.t.reader, "native").await.number, 43);
    let c =
        w.s.list_issue_comments(req(
            &w.t.reader,
            ListIssueCommentsRequest {
                repo: rref("acme/app"),
                number: 42,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(c.comments[0].created_at.unwrap().seconds, 1_600_000_100);
    // Imports emit no events (a migration must not flood webhooks) but are audited.
    let kinds = event_kinds(&w).await;
    assert_eq!(kinds.iter().filter(|k| *k == "issue.opened").count(), 1);
    assert!(
        audit_entries(&w)
            .await
            .iter()
            .any(|(a, _)| a == "issue.import")
    );
}

// -------------------------------------------------------------- projects

fn pv_text(field: &str, v: &str) -> ProjectFieldValue {
    ProjectFieldValue {
        field_id: field.into(),
        value: Some(project_field_value::Value::Text(v.into())),
        ..Default::default()
    }
}

async fn make_project(w: &W, tok: &str, title: &str, public: bool) -> Project {
    w.s.create_project(req(
        tok,
        CreateProjectRequest {
            owner: "acme".into(),
            title: title.into(),
            public,
            link_repos: vec![RepoRef {
                repo: Some(repo_ref::Repo::FullName("acme/app".into())),
            }],
            ..Default::default()
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn fields_of(w: &W, tok: &str, project: &str) -> Vec<ProjectField> {
    w.s.list_project_fields(req(
        tok,
        ListProjectFieldsRequest {
            project_id: project.into(),
            page: None,
        },
    ))
    .await
    .unwrap()
    .into_inner()
    .fields
}

fn field<'a>(fs: &'a [ProjectField], name: &str) -> &'a ProjectField {
    fs.iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

fn opt_id(f: &ProjectField, name: &str) -> String {
    f.options
        .iter()
        .find(|o| o.name == name)
        .unwrap()
        .id
        .clone()
}

async fn add_issue_item(w: &W, tok: &str, project: &str, issue: &Issue) -> ProjectItem {
    w.s.add_project_item(req(
        tok,
        AddProjectItemRequest {
            project_id: project.into(),
            content: Some(SubjectRef {
                kind: SubjectKind::Issue as i32,
                repo: rref("acme/app"),
                id: issue.id.clone(),
            }),
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn items(w: &W, tok: &str, project: &str, query: &str) -> Vec<String> {
    let r =
        w.s.list_project_items(req(
            tok,
            ListProjectItemsRequest {
                project_id: project.into(),
                query: query.into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.total_count as usize, r.items.len());
    r.items.iter().map(|i| i.title.clone()).collect()
}

fn status_of(item: &ProjectItem) -> String {
    item.field_values
        .iter()
        .find(|v| v.field_name == "Status")
        .map(|v| v.display_value.clone())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn projects_items_fields_views_and_filters() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    // Any org member creates; outsiders cannot; defaults come with the project.
    assert_eq!(
        reason(
            s.create_project(req(
                &t.outsider,
                CreateProjectRequest {
                    owner: "acme".into(),
                    title: "x".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let p = make_project(&w, &t.reader, "Roadmap", false).await;
    assert_eq!(
        (p.number, p.owner_login.as_str(), p.item_count),
        (1, "acme", 0)
    );
    assert_eq!(p.linked_repo_ids, vec![w.repo.id.clone()]);
    assert_eq!(p.url, "/orgs/acme/projects/1");
    let fs = fields_of(&w, &t.reader, &p.id).await;
    assert!(fs.iter().any(|f| f.built_in && f.name == "Title"));
    let status = field(&fs, "Status").clone();
    assert_eq!(
        status
            .options
            .iter()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Todo", "In Progress", "Done"]
    );
    let wfs = s
        .list_project_workflows(req(
            &t.reader,
            ListProjectWorkflowsRequest {
                project_id: p.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .workflows;
    assert_eq!(wfs.len(), 10);
    assert!(wfs.iter().all(|w| w.is_default));
    assert_eq!(
        s.list_project_views(req(
            &t.reader,
            ListProjectViewsRequest {
                project_id: p.id.clone(),
                page: None
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .views
        .len(),
        1
    );
    assert_eq!(
        s.get_project_by_number(req(
            &t.reader,
            GetProjectByNumberRequest {
                owner: "acme".into(),
                number: 1
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .id,
        p.id
    );
    // Custom fields: kinds validate.
    let effort = s
        .create_project_field(req(
            &t.writer,
            CreateProjectFieldRequest {
                project_id: p.id.clone(),
                name: "Effort".into(),
                kind: ProjectFieldKind::Number as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let notes = s
        .create_project_field(req(
            &t.writer,
            CreateProjectFieldRequest {
                project_id: p.id.clone(),
                name: "Notes".into(),
                kind: ProjectFieldKind::Text as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        reason(
            s.create_project_field(req(
                &t.writer,
                CreateProjectFieldRequest {
                    project_id: p.id.clone(),
                    name: "effort".into(),
                    kind: ProjectFieldKind::Number as i32,
                    ..Default::default()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    assert_eq!(
        reason(
            s.create_project_field(req(
                &t.writer,
                CreateProjectFieldRequest {
                    project_id: p.id.clone(),
                    name: "Labels2".into(),
                    kind: ProjectFieldKind::Labels as i32,
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    // Items: issues, drafts, duplicates.
    let i1 = open(&w, &t.writer, "Crash on login").await;
    let i2 = open(&w, &t.writer, "Docs typo").await;
    s.add_issue_labels(req(
        &t.writer,
        AddIssueLabelsRequest {
            repo: rref("acme/app"),
            number: 1,
            labels: vec!["bug".into()],
            create_missing: false,
        },
    ))
    .await
    .unwrap();
    let it1 = add_issue_item(&w, &t.writer, &p.id, &i1).await;
    let it2 = add_issue_item(&w, &t.writer, &p.id, &i2).await;
    assert_eq!(it1.kind, ProjectItemKind::Issue as i32);
    assert_eq!(it1.repo_full_name, "acme/app");
    assert_eq!(status_of(&it1), "Todo"); // the default "item added" workflow
    let dup = s
        .add_project_item(req(
            &t.writer,
            AddProjectItemRequest {
                project_id: p.id.clone(),
                content: Some(SubjectKind::Issue).map(|k| SubjectRef {
                    kind: k as i32,
                    repo: rref("acme/app"),
                    id: "1".into(),
                }),
            },
        ))
        .await;
    assert_eq!(reason(dup), "ALREADY_EXISTS");
    let draft = s
        .create_project_draft_item(req(
            &t.writer,
            CreateProjectDraftItemRequest {
                project_id: p.id.clone(),
                title: "Think about caching".into(),
                body: "later".into(),
                assignees: vec!["writer".into()],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(draft.kind, ProjectItemKind::Draft as i32);
    assert_eq!(draft.content_state, "draft");
    let proj = s
        .get_project(req(
            &t.reader,
            GetProjectRequest {
                project_id: p.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(proj.item_count, 3);
    // Values and filters.
    let set = |item: &str, v: ProjectFieldValue, version: u64| {
        s.set_project_item_field_value(req(
            &t.writer,
            SetProjectItemFieldValueRequest {
                project_id: p.id.clone(),
                item_id: item.into(),
                value: Some(v),
                expected_version: version,
            },
        ))
    };
    set(
        &it1.id,
        ProjectFieldValue {
            field_id: effort.id.clone(),
            value: Some(project_field_value::Value::Number(5.0)),
            ..Default::default()
        },
        0,
    )
    .await
    .unwrap();
    set(
        &it2.id,
        ProjectFieldValue {
            field_id: effort.id.clone(),
            value: Some(project_field_value::Value::Number(1.0)),
            ..Default::default()
        },
        0,
    )
    .await
    .unwrap();
    let in_progress = set(
        &it1.id,
        ProjectFieldValue {
            field_id: status.id.clone(),
            value: Some(project_field_value::Value::OptionId("in progress".into())),
            ..Default::default()
        },
        0,
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(status_of(&in_progress), "In Progress");
    assert_eq!(
        reason(set(&it1.id, pv_text(&notes.id, "x"), 1).await),
        "VERSION_MISMATCH"
    );
    assert_eq!(
        reason(set(&it1.id, pv_text(&effort.id, "five"), 0).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(
            set(
                &it1.id,
                pv_text(&fs.iter().find(|f| f.name == "Title").unwrap().id, "x"),
                0
            )
            .await
        ),
        "INVALID_ARGUMENT"
    );
    set(&it1.id, pv_text(&notes.id, "owner: writer"), 0)
        .await
        .unwrap();
    assert_eq!(
        items(&w, &t.reader, &p.id, "").await,
        vec!["Crash on login", "Docs typo", "Think about caching"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "status:\"In Progress\"").await,
        vec!["Crash on login"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "status:todo,done").await,
        vec!["Docs typo", "Think about caching"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "no:status").await,
        Vec::<String>::new()
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "effort:>=2").await,
        vec!["Crash on login"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "effort:1..5 -is:draft").await,
        vec!["Crash on login", "Docs typo"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "is:draft").await,
        vec!["Think about caching"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "label:bug").await,
        vec!["Crash on login"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "assignee:@me").await,
        Vec::<String>::new()
    );
    assert_eq!(
        items(&w, &t.writer, &p.id, "assignee:@me").await,
        vec!["Think about caching"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "assignee:writer-login").await,
        vec!["Think about caching"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "repo:acme/app is:issue").await,
        vec!["Crash on login", "Docs typo"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "notes:writer").await,
        vec!["Crash on login"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "login").await,
        vec!["Crash on login"]
    );
    assert_eq!(
        reason(
            s.list_project_items(req(
                &t.reader,
                ListProjectItemsRequest {
                    project_id: p.id.clone(),
                    query: "bogus:1".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    // Saved views: board by status, sorted table, roadmap.
    let board = s
        .create_project_view(req(
            &t.writer,
            CreateProjectViewRequest {
                project_id: p.id.clone(),
                name: "Board".into(),
                layout: ProjectViewLayout::Board as i32,
                filter: "is:issue".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(board.column_field_id, status.id);
    assert_eq!(board.number, 2);
    let table = s
        .create_project_view(req(
            &t.writer,
            CreateProjectViewRequest {
                project_id: p.id.clone(),
                name: "By effort".into(),
                sort_by: vec![ProjectViewSort {
                    field_id: effort.id.clone(),
                    descending: true,
                }],
                sum_field_ids: vec![effort.id.clone()],
                visible_field_ids: vec![effort.id.clone()],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        reason(
            s.create_project_view(req(
                &t.writer,
                CreateProjectViewRequest {
                    project_id: p.id.clone(),
                    name: "bad".into(),
                    sum_field_ids: vec![notes.id.clone()],
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(
            s.create_project_view(req(
                &t.writer,
                CreateProjectViewRequest {
                    project_id: p.id.clone(),
                    name: "bad".into(),
                    filter: "zzz:1".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let by_view = s
        .list_project_items(req(
            &t.reader,
            ListProjectItemsRequest {
                project_id: p.id.clone(),
                view_id: table.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        by_view
            .items
            .iter()
            .map(|i| i.title.clone())
            .collect::<Vec<_>>(),
        vec!["Crash on login", "Docs typo", "Think about caching"]
    );
    let by_board = s
        .list_project_items(req(
            &t.reader,
            ListProjectItemsRequest {
                project_id: p.id.clone(),
                view_id: board.number.to_string(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_board.total_count, 2);
    let roadmap = s
        .create_project_view(req(
            &t.writer,
            CreateProjectViewRequest {
                project_id: p.id.clone(),
                name: "Plan".into(),
                layout: ProjectViewLayout::Roadmap as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(roadmap.roadmap_zoom, ProjectRoadmapZoom::Month as i32);
    let renamed = s
        .update_project_view(req(
            &t.writer,
            UpdateProjectViewRequest {
                project_id: p.id.clone(),
                view_id: roadmap.id.clone(),
                name: Some("Timeline".into()),
                roadmap_zoom: ProjectRoadmapZoom::Quarter as i32,
                expected_version: 1,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (renamed.name.as_str(), renamed.roadmap_zoom, renamed.version),
        ("Timeline", ProjectRoadmapZoom::Quarter as i32, 2)
    );
    s.delete_project_view(req(
        &t.writer,
        DeleteProjectViewRequest {
            project_id: p.id.clone(),
            view_id: roadmap.id.clone(),
        },
    ))
    .await
    .unwrap();
    // Manual order, archive, delete.
    let moved = s
        .move_project_item(req(
            &t.writer,
            MoveProjectItemRequest {
                project_id: p.id.clone(),
                item_id: draft.id.clone(),
                after_item_id: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(moved.position < it1.position);
    assert_eq!(
        items(&w, &t.reader, &p.id, "").await[0],
        "Think about caching"
    );
    s.move_project_item(req(
        &t.writer,
        MoveProjectItemRequest {
            project_id: p.id.clone(),
            item_id: draft.id.clone(),
            after_item_id: it1.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        items(&w, &t.reader, &p.id, "").await,
        vec!["Crash on login", "Think about caching", "Docs typo"]
    );
    let arch = s
        .archive_project_item(req(
            &t.writer,
            ArchiveProjectItemRequest {
                project_id: p.id.clone(),
                item_id: it2.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(arch.archived);
    assert_eq!(items(&w, &t.reader, &p.id, "").await.len(), 2);
    let with_archived = s
        .list_project_items(req(
            &t.reader,
            ListProjectItemsRequest {
                project_id: p.id.clone(),
                include_archived: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(with_archived.items.len(), 3);
    let only_archived = s
        .list_project_items(req(
            &t.reader,
            ListProjectItemsRequest {
                project_id: p.id.clone(),
                archived_only: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(only_archived.items.len(), 1);
    assert_eq!(
        s.get_project(req(
            &t.reader,
            GetProjectRequest {
                project_id: p.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .item_count,
        2
    );
    s.unarchive_project_item(req(
        &t.writer,
        UnarchiveProjectItemRequest {
            project_id: p.id.clone(),
            item_id: it2.id.clone(),
        },
    ))
    .await
    .unwrap();
    s.delete_project_item(req(
        &t.writer,
        DeleteProjectItemRequest {
            project_id: p.id.clone(),
            item_id: it2.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        s.get_project(req(
            &t.reader,
            GetProjectRequest {
                project_id: p.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .item_count,
        2
    );
    // The issue is untouched and records the project on its timeline.
    let tl = kinds(&timeline(&w, 2).await);
    assert!(tl.contains(&IssueTimelineEventKind::AddedToProject));
    assert!(tl.contains(&IssueTimelineEventKind::RemovedFromProject));
    // Draft edit and convert to an issue.
    let upd = s
        .update_project_draft_item(req(
            &t.writer,
            UpdateProjectDraftItemRequest {
                project_id: p.id.clone(),
                item_id: draft.id.clone(),
                title: Some("Cache the hot path".into()),
                body: None,
                assignees: vec![],
                set_assignees: true,
                expected_version: 0,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(upd.title, "Cache the hot path");
    assert!(upd.assignees.is_empty());
    let converted = s
        .convert_project_draft_item(req(
            &t.writer,
            ConvertProjectDraftItemRequest {
                project_id: p.id.clone(),
                item_id: draft.id.clone(),
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(converted.kind, ProjectItemKind::Issue as i32);
    assert_eq!(converted.id, draft.id);
    assert_eq!(converted.repo_full_name, "acme/app");
    assert_eq!(converted.number, 3);
    assert_eq!(get(&w, &t.reader, 3).await.title, "Cache the hot path");
    // Item snapshots follow the issue.
    s.update_issue(req(
        &t.writer,
        UpdateIssueRequest {
            repo: rref("acme/app"),
            number: 3,
            title: Some("Cache everything".into()),
            body: None,
            expected_version: 0,
        },
    ))
    .await
    .unwrap();
    assert!(
        items(&w, &t.reader, &p.id, "")
            .await
            .contains(&"Cache everything".to_string())
    );
    // Settings, closing, public flag, templates, copies.
    assert_eq!(
        reason(
            s.update_project(req(
                &t.writer,
                UpdateProjectRequest {
                    project_id: p.id.clone(),
                    title: Some("Mine".into()),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    let upd = s
        .update_project(req(
            &t.reader,
            UpdateProjectRequest {
                project_id: p.id.clone(),
                title: Some("Roadmap 2026".into()),
                is_template: Some(true),
                ..Default::default()
            },
        ))
        .await;
    // The creator administers their own project.
    assert_eq!(upd.unwrap().into_inner().title, "Roadmap 2026");
    let copy = s
        .copy_project(req(
            &t.writer,
            CopyProjectRequest {
                source_project_id: p.id.clone(),
                owner: "acme".into(),
                title: "Copy".into(),
                include_draft_items: true,
                public: false,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(copy.number, 2);
    assert_eq!(copy.copied_from_project_id, p.id);
    let cfs = fields_of(&w, &t.writer, &copy.id).await;
    assert_eq!(field(&cfs, "Effort").kind, ProjectFieldKind::Number as i32);
    assert_ne!(field(&cfs, "Status").id, status.id);
    assert_eq!(
        s.list_project_workflows(req(
            &t.writer,
            ListProjectWorkflowsRequest {
                project_id: copy.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .workflows
        .len(),
        10
    );
    assert_eq!(
        s.list_project_views(req(
            &t.writer,
            ListProjectViewsRequest {
                project_id: copy.id.clone(),
                page: None
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .views
        .len(),
        3
    );
    assert_eq!(items(&w, &t.writer, &copy.id, "").await.len(), 0);
    let templates = s
        .list_projects(req(
            &t.reader,
            ListProjectsRequest {
                owner: "acme".into(),
                templates_only: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(templates.projects.len(), 1);
    let linked = s
        .list_projects(req(
            &t.reader,
            ListProjectsRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(linked.projects.len(), 1);
    s.update_project(req(
        &t.admin,
        UpdateProjectRequest {
            project_id: copy.id.clone(),
            closed: Some(true),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        s.list_projects(req(
            &t.reader,
            ListProjectsRequest {
                owner: "acme".into(),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .projects
        .len(),
        1
    );
    assert_eq!(
        s.list_projects(req(
            &t.reader,
            ListProjectsRequest {
                owner: "acme".into(),
                include_closed: true,
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .projects
        .len(),
        2
    );
    assert_eq!(
        reason(
            s.create_project_draft_item(req(
                &t.writer,
                CreateProjectDraftItemRequest {
                    project_id: copy.id.clone(),
                    title: "t".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Deleting a project removes its rows but not the issues.
    s.delete_project(req(
        &t.admin,
        DeleteProjectRequest {
            project_id: copy.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            s.get_project(req(
                &t.admin,
                GetProjectRequest {
                    project_id: copy.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(get(&w, &t.reader, 1).await.number, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_visibility_permissions_and_redaction() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    // A private repo the reader cannot see, with an issue.
    let secret = s
        .create_repo(req(
            &t.owner,
            CreateRepoRequest {
                org: "acme".into(),
                name: "secret".into(),
                visibility: Visibility::Private as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    s.add_collaborator(req(
        &t.owner,
        AddCollaboratorRequest {
            repo: rref("acme/secret"),
            principal_id: "writer".into(),
            role: RepoRole::Write as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    s.update_org_settings(req(
        &t.owner,
        UpdateOrgSettingsRequest {
            org: "acme".into(),
            base_repo_role: Some(RepoRole::None as i32),
            ..Default::default()
        },
    ))
    .await
    .ok();
    let hidden = s
        .create_issue(req(
            &t.writer,
            CreateIssueRequest {
                repo: rref("acme/secret"),
                title: "Confidential".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let public = make_project(&w, &t.admin, "Public board", true).await;
    let private = make_project(&w, &t.admin, "Private board", false).await;
    // Items of readable and unreadable content.
    let visible_issue = open(&w, &t.writer, "Visible").await;
    add_issue_item(&w, &t.writer, &public.id, &visible_issue).await;
    s.add_project_item(req(
        &t.writer,
        AddProjectItemRequest {
            project_id: public.id.clone(),
            content: Some(SubjectRef {
                kind: SubjectKind::Issue as i32,
                repo: rref("acme/secret"),
                id: hidden.id.clone(),
            }),
        },
    ))
    .await
    .unwrap();
    // Outsiders read public projects only; they see item content they may read.
    assert!(
        s.get_project(req(
            &t.outsider,
            GetProjectRequest {
                project_id: public.id.clone()
            }
        ))
        .await
        .is_ok()
    );
    assert_eq!(
        reason(
            s.get_project(req(
                &t.outsider,
                GetProjectRequest {
                    project_id: private.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let seen = s
        .list_project_items(req(
            &t.outsider,
            ListProjectItemsRequest {
                project_id: public.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(seen.items.len(), 2);
    assert_eq!(seen.items[0].kind, ProjectItemKind::Redacted as i32);
    assert_eq!(seen.items[1].kind, ProjectItemKind::Redacted as i32);
    assert!(seen.items[1].title.is_empty() && seen.items[1].content.is_none());
    // The writer reads the secret one; nobody but members writes.
    let full = s
        .list_project_items(req(
            &t.writer,
            ListProjectItemsRequest {
                project_id: public.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(full.items[1].title, "Confidential");
    assert_eq!(full.items[1].repo_full_name, "acme/secret");
    assert_eq!(
        reason(
            s.create_project_draft_item(req(
                &t.outsider,
                CreateProjectDraftItemRequest {
                    project_id: public.id.clone(),
                    title: "x".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(
            s.create_project_draft_item(req(
                &t.outsider,
                CreateProjectDraftItemRequest {
                    project_id: private.id.clone(),
                    title: "x".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // A read-only token cannot write; a repo-restricted token cannot either.
    let ro = w.env.token("admin", actions(&[Action::Read]));
    assert_eq!(
        reason(
            s.create_project_draft_item(req(
                &ro,
                CreateProjectDraftItemRequest {
                    project_id: public.id.clone(),
                    title: "x".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "POLICY_DENIED"
    );
    assert!(
        s.get_project(req(
            &ro,
            GetProjectRequest {
                project_id: private.id.clone()
            }
        ))
        .await
        .is_ok()
    );
    let restricted = w
        .env
        .token("admin", Scope::new().repos([secret.id.clone()]).unwrap());
    assert_eq!(
        reason(
            s.create_project_draft_item(req(
                &restricted,
                CreateProjectDraftItemRequest {
                    project_id: public.id.clone(),
                    title: "x".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "POLICY_DENIED"
    );
    // Only owners and admins (or the project's creator) administer.
    assert_eq!(
        reason(
            s.delete_project(req(
                &t.writer,
                DeleteProjectRequest {
                    project_id: private.id.clone()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(
            s.update_project(req(
                &t.writer,
                UpdateProjectRequest {
                    project_id: private.id.clone(),
                    public: Some(true),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert!(
        s.delete_project(req(
            &t.owner,
            DeleteProjectRequest {
                project_id: private.id.clone()
            }
        ))
        .await
        .is_ok()
    );
    // Denied project changes are audited.
    let audit = audit_entries(&w).await;
    assert!(
        audit
            .iter()
            .any(|(a, r)| a == "project.delete" && r == "PERMISSION_DENIED")
    );
    assert!(
        audit
            .iter()
            .any(|(a, r)| a == "project_item.add" && r == "ok")
    );
    // User-owned projects belong to that user.
    let mine = s
        .create_project(req(
            &t.writer,
            CreateProjectRequest {
                owner: "writer".into(),
                title: "Personal".into(),
                public: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mine.owner_kind, ProjectOwnerKind::User as i32);
    assert_eq!(mine.url, "/users/writer-login/projects/1");
    assert!(
        s.get_project(req(
            &t.outsider,
            GetProjectRequest {
                project_id: mine.id.clone()
            }
        ))
        .await
        .is_ok()
    );
    assert_eq!(
        reason(
            s.delete_project(req(
                &t.outsider,
                DeleteProjectRequest {
                    project_id: mine.id.clone()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(
            s.create_project(req(
                &t.reader,
                CreateProjectRequest {
                    owner: "writer".into(),
                    title: "x".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert!(
        s.delete_project(req(
            &t.writer,
            DeleteProjectRequest {
                project_id: mine.id.clone()
            }
        ))
        .await
        .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_automations_react_to_state_changes_in_the_same_transaction() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let forge = &w.env.forge;
    let p = make_project(&w, &t.admin, "Flow", false).await;
    let fs = fields_of(&w, &t.admin, &p.id).await;
    let status = field(&fs, "Status").clone();
    let i = open(&w, &t.writer, "Automate me").await;
    let item = add_issue_item(&w, &t.writer, &p.id, &i).await;
    assert_eq!(status_of(&item), "Todo");
    // Closing the issue moves the item to Done; reopening moves it back (to In Progress).
    close(&w, &t.writer, 1, IssueStateReason::Completed).await;
    let cur = || {
        w.s.get_project_item(req(
            &w.t.reader,
            GetProjectItemRequest {
                project_id: p.id.clone(),
                item_id: item.id.clone(),
            },
        ))
    };
    assert_eq!(status_of(&cur().await.unwrap().into_inner()), "Done");
    s.reopen_issue(req(
        &t.writer,
        ReopenIssueRequest {
            repo: rref("acme/app"),
            number: 1,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(status_of(&cur().await.unwrap().into_inner()), "In Progress");
    assert!(kinds(&timeline(&w, 1).await).contains(&IssueTimelineEventKind::ProjectStatusChanged));
    // Disabling a workflow stops it; enabling validates it.
    let wfs = s
        .list_project_workflows(req(
            &t.admin,
            ListProjectWorkflowsRequest {
                project_id: p.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .workflows;
    let closed_wf = wfs
        .iter()
        .find(|w| w.kind == ProjectWorkflowKind::ItemClosed as i32)
        .unwrap();
    s.disable_project_workflow(req(
        &t.admin,
        DisableProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: closed_wf.id.clone(),
        },
    ))
    .await
    .unwrap();
    close(&w, &t.writer, 1, IssueStateReason::Completed).await;
    assert_eq!(status_of(&cur().await.unwrap().into_inner()), "In Progress");
    s.enable_project_workflow(req(
        &t.admin,
        EnableProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: closed_wf.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            s.delete_project_workflow(req(
                &t.admin,
                DeleteProjectWorkflowRequest {
                    project_id: p.id.clone(),
                    workflow_id: closed_wf.id.clone()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Only project admins touch workflows.
    assert_eq!(
        reason(
            s.disable_project_workflow(req(
                &t.reader,
                DisableProjectWorkflowRequest {
                    project_id: p.id.clone(),
                    workflow_id: closed_wf.id.clone()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );

    // Auto-close: reaching Done closes the issue.
    let i2 = open(&w, &t.writer, "Close by status").await;
    let item2 = add_issue_item(&w, &t.writer, &p.id, &i2).await;
    let auto_close = wfs
        .iter()
        .find(|w| w.kind == ProjectWorkflowKind::AutoCloseIssue as i32)
        .unwrap();
    s.enable_project_workflow(req(
        &t.admin,
        EnableProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: auto_close.id.clone(),
        },
    ))
    .await
    .unwrap();
    s.set_project_item_field_value(req(
        &t.writer,
        SetProjectItemFieldValueRequest {
            project_id: p.id.clone(),
            item_id: item2.id.clone(),
            value: Some(ProjectFieldValue {
                field_id: status.id.clone(),
                value: Some(project_field_value::Value::OptionId(opt_id(
                    &status, "Done",
                ))),
                ..Default::default()
            }),
            expected_version: 0,
        },
    ))
    .await
    .unwrap();
    assert_eq!(get(&w, &t.reader, 2).await.state, IssueState::Closed as i32);
    // Auto-add: a new issue in a linked repo that matches the filter joins the project.
    let auto_add = wfs
        .iter()
        .find(|w| w.kind == ProjectWorkflowKind::AutoAdd as i32)
        .unwrap();
    assert_eq!(
        reason(
            s.enable_project_workflow(req(
                &t.admin,
                EnableProjectWorkflowRequest {
                    project_id: p.id.clone(),
                    workflow_id: auto_add.id.clone()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    ); // no source repo yet
    s.update_project_workflow(req(
        &t.admin,
        UpdateProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: auto_add.id.clone(),
            filter: Some("is:issue label:bug".into()),
            source_repo: rref("acme/app"),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    s.enable_project_workflow(req(
        &t.admin,
        EnableProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: auto_add.id.clone(),
        },
    ))
    .await
    .unwrap();
    let before = items(&w, &t.admin, &p.id, "").await.len();
    open(&w, &t.writer, "Not a bug").await;
    assert_eq!(items(&w, &t.admin, &p.id, "").await.len(), before);
    s.create_issue(req(
        &t.writer,
        CreateIssueRequest {
            repo: rref("acme/app"),
            title: "A bug".into(),
            labels: vec!["bug".into()],
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(items(&w, &t.admin, &p.id, "").await.len(), before + 1);
    assert_eq!(
        items(&w, &t.admin, &p.id, "").await.last().unwrap(),
        "A bug"
    );
    // Auto-archive sweeps quiet matching items.
    let archive = wfs
        .iter()
        .find(|w| w.kind == ProjectWorkflowKind::AutoArchive as i32)
        .unwrap();
    s.update_project_workflow(req(
        &t.admin,
        UpdateProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: archive.id.clone(),
            filter: Some("is:closed".into()),
            archive_after_days: Some(0),
            ..Default::default()
        },
    ))
    .await
    .unwrap_err();
    s.update_project_workflow(req(
        &t.admin,
        UpdateProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: archive.id.clone(),
            filter: Some("is:closed".into()),
            archive_after_days: Some(1),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    s.enable_project_workflow(req(
        &t.admin,
        EnableProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: archive.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(forge.work.sweep_projects(&Cx::system()).await.unwrap(), 0); // too recent
    // Change request items follow their CR through workflows.
    let n = forge
        .work
        .allocate_number(&Cx::system(), &w.repo.id, NumberKind::ChangeRequest, "CR-X")
        .await
        .unwrap();
    let cr = s
        .add_project_item(req(
            &t.writer,
            AddProjectItemRequest {
                project_id: p.id.clone(),
                content: Some(SubjectRef {
                    kind: SubjectKind::ChangeRequest as i32,
                    repo: rref("acme/app"),
                    id: n.to_string(),
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (cr.kind, cr.number),
        (ProjectItemKind::ChangeRequest as i32, n)
    );
    assert_eq!(status_of(&cr), "Todo");
    forge
        .events
        .append(
            &Cx::system(),
            change_event(
                &w,
                "cr.opened",
                "CR-X",
                n,
                "",
                Principal {
                    id: "writer".into(),
                    kind: PrincipalKind::Human as i32,
                    ..Default::default()
                },
            ),
            "o",
        )
        .await
        .unwrap();
    let after_open = s
        .get_project_item(req(
            &t.admin,
            GetProjectItemRequest {
                project_id: p.id.clone(),
                item_id: cr.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(after_open.title, "Fix the crash");
    forge
        .events
        .append(
            &Cx::system(),
            change_event(
                &w,
                "cr.landed",
                "CR-X",
                n,
                "",
                Principal {
                    id: "writer".into(),
                    kind: PrincipalKind::Human as i32,
                    ..Default::default()
                },
            ),
            "l",
        )
        .await
        .unwrap();
    let after_land = s
        .get_project_item(req(
            &t.admin,
            GetProjectItemRequest {
                project_id: p.id.clone(),
                item_id: cr.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status_of(&after_land), "Done");
    assert_eq!(after_land.content_state, "landed");
    assert_eq!(
        items(&w, &t.admin, &p.id, "is:merged").await,
        vec!["Fix the crash"]
    );
    // Review events drive the review workflows.
    let changes_requested = wfs
        .iter()
        .find(|w| w.kind == ProjectWorkflowKind::ChangesRequested as i32)
        .unwrap();
    s.enable_project_workflow(req(
        &t.admin,
        EnableProjectWorkflowRequest {
            project_id: p.id.clone(),
            workflow_id: changes_requested.id.clone(),
        },
    ))
    .await
    .unwrap();
    forge
        .events
        .append(
            &Cx::system(),
            Event {
                kind: "review.submitted".into(),
                repo_id: w.repo.id.clone(),
                org_id: w.repo.org_id.clone(),
                body: Some(event::Body::Review(Review {
                    id: "R1".into(),
                    change_id: "CR-X".into(),
                    verdict: Verdict::RequestChanges as i32,
                    ..Default::default()
                })),
                ..Default::default()
            },
            "r",
        )
        .await
        .unwrap();
    let after_review = s
        .get_project_item(req(
            &t.admin,
            GetProjectItemRequest {
                project_id: p.id.clone(),
                item_id: cr.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status_of(&after_review), "In Progress");
    // The project's changes are events and audit entries.
    let kinds = event_kinds(&w).await;
    assert!(kinds.is_empty() || kinds.iter().all(|k| !k.is_empty()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_iterations_charts_status_updates_and_links() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    let p = make_project(&w, &t.admin, "Plan", false).await;
    let fs = fields_of(&w, &t.admin, &p.id).await;
    let status = field(&fs, "Status").clone();
    // An iteration field with a default length.
    let sprint = s
        .create_project_field(req(
            &t.writer,
            CreateProjectFieldRequest {
                project_id: p.id.clone(),
                name: "Sprint".into(),
                kind: ProjectFieldKind::Iteration as i32,
                iteration_config: Some(ProjectIterationConfig {
                    start_on: Some(pbjson_types::Timestamp {
                        seconds: 1_700_000_000,
                        nanos: 0,
                    }),
                    duration_days: 7,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let add_it = |title: &str| {
        s.add_project_iteration(req(
            &t.writer,
            AddProjectIterationRequest {
                project_id: p.id.clone(),
                field_id: sprint.id.clone(),
                title: title.into(),
                ..Default::default()
            },
        ))
    };
    let s1 = add_it("").await.unwrap().into_inner();
    let s2 = add_it("Sprint two").await.unwrap().into_inner();
    assert_eq!((s1.title.as_str(), s1.duration_days), ("Iteration 1", 7));
    assert_eq!(s2.start_on.unwrap().seconds, 1_700_000_000 + 7 * 86_400);
    // Items in sprints; completing moves open items on.
    let i1 = open(&w, &t.writer, "one").await;
    let i2 = open(&w, &t.writer, "two").await;
    let it1 = add_issue_item(&w, &t.writer, &p.id, &i1).await;
    let it2 = add_issue_item(&w, &t.writer, &p.id, &i2).await;
    for it in [&it1, &it2] {
        s.set_project_item_field_value(req(
            &t.writer,
            SetProjectItemFieldValueRequest {
                project_id: p.id.clone(),
                item_id: it.id.clone(),
                value: Some(ProjectFieldValue {
                    field_id: sprint.id.clone(),
                    value: Some(project_field_value::Value::IterationId(s1.title.clone())),
                    ..Default::default()
                }),
                expected_version: 0,
            },
        ))
        .await
        .unwrap();
    }
    close(&w, &t.writer, 2, IssueStateReason::Completed).await;
    assert_eq!(
        items(&w, &t.reader, &p.id, "sprint:\"Iteration 1\"")
            .await
            .len(),
        2
    );
    assert_eq!(
        reason(
            s.complete_project_iteration(req(
                &t.writer,
                CompleteProjectIterationRequest {
                    project_id: p.id.clone(),
                    field_id: sprint.id.clone(),
                    iteration_id: s1.id.clone(),
                    move_open_items_to_iteration_id: s1.id.clone()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let done = s
        .complete_project_iteration(req(
            &t.writer,
            CompleteProjectIterationRequest {
                project_id: p.id.clone(),
                field_id: sprint.id.clone(),
                iteration_id: s1.id.clone(),
                move_open_items_to_iteration_id: s2.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(done.completed);
    assert_eq!(
        items(&w, &t.reader, &p.id, "sprint:\"Sprint two\"").await,
        vec!["one"]
    );
    assert_eq!(
        items(&w, &t.reader, &p.id, "sprint:\"Iteration 1\"").await,
        vec!["two"]
    );
    let active = s
        .list_project_iterations(req(
            &t.reader,
            ListProjectIterationsRequest {
                project_id: p.id.clone(),
                field_id: sprint.id.clone(),
                include_completed: false,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .iterations;
    assert_eq!(active.len(), 1);
    let all = s
        .list_project_iterations(req(
            &t.reader,
            ListProjectIterationsRequest {
                project_id: p.id.clone(),
                field_id: sprint.id.clone(),
                include_completed: true,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .iterations;
    assert_eq!(all.len(), 2);
    assert_eq!(
        reason(
            s.list_project_iterations(req(
                &t.reader,
                ListProjectIterationsRequest {
                    project_id: p.id.clone(),
                    field_id: status.id.clone(),
                    include_completed: true
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    // Insights: counts over a field, grouped, with sums.
    let effort = s
        .create_project_field(req(
            &t.writer,
            CreateProjectFieldRequest {
                project_id: p.id.clone(),
                name: "Effort".into(),
                kind: ProjectFieldKind::Number as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    for (it, e) in [(&it1, 3.0), (&it2, 5.0)] {
        s.set_project_item_field_value(req(
            &t.writer,
            SetProjectItemFieldValueRequest {
                project_id: p.id.clone(),
                item_id: it.id.clone(),
                value: Some(ProjectFieldValue {
                    field_id: effort.id.clone(),
                    value: Some(project_field_value::Value::Number(e)),
                    ..Default::default()
                }),
                expected_version: 0,
            },
        ))
        .await
        .unwrap();
    }
    let chart = s
        .create_project_chart(req(
            &t.writer,
            CreateProjectChartRequest {
                project_id: p.id.clone(),
                chart: Some(ProjectChart {
                    name: "By status".into(),
                    x_axis_field_id: status.id.clone(),
                    ..Default::default()
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(chart.layout, ProjectChartLayout::Column as i32);
    let q = s
        .query_project_chart(req(
            &t.reader,
            QueryProjectChartRequest {
                project_id: p.id.clone(),
                chart_id: chart.id.clone(),
                definition: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(q.series.len(), 1);
    let pts: Vec<(String, f64)> = q.series[0]
        .points
        .iter()
        .map(|p| (p.label.clone(), p.value))
        .collect();
    assert_eq!(
        pts,
        vec![("Todo".to_string(), 1.0), ("Done".to_string(), 1.0)]
    );
    let sum = s
        .query_project_chart(req(
            &t.reader,
            QueryProjectChartRequest {
                project_id: p.id.clone(),
                chart_id: String::new(),
                definition: Some(ProjectChart {
                    x_axis_field_id: sprint.id.clone(),
                    group_by_field_id: status.id.clone(),
                    aggregate: ProjectChartAggregate::Sum as i32,
                    y_axis_field_id: effort.id.clone(),
                    ..Default::default()
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let total: f64 = sum
        .series
        .iter()
        .flat_map(|s| s.points.iter())
        .map(|p| p.value)
        .sum();
    assert_eq!(total, 8.0);
    assert_eq!(
        reason(
            s.create_project_chart(req(
                &t.writer,
                CreateProjectChartRequest {
                    project_id: p.id.clone(),
                    chart: Some(ProjectChart {
                        name: "bad".into(),
                        x_axis_field_id: status.id.clone(),
                        aggregate: ProjectChartAggregate::Sum as i32,
                        y_axis_field_id: status.id.clone(),
                        ..Default::default()
                    })
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let hist = s
        .query_project_chart(req(
            &t.reader,
            QueryProjectChartRequest {
                project_id: p.id.clone(),
                chart_id: String::new(),
                definition: Some(ProjectChart {
                    historical: true,
                    ..Default::default()
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let last = hist.series[0].points.last().unwrap();
    assert_eq!(last.value, 2.0);
    assert!(last.at.is_some());
    s.delete_project_chart(req(
        &t.writer,
        DeleteProjectChartRequest {
            project_id: p.id.clone(),
            chart_id: chart.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert!(
        s.list_project_charts(req(
            &t.reader,
            ListProjectChartsRequest {
                project_id: p.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .charts
        .is_empty()
    );
    // Status updates.
    let u1 = s
        .create_project_status_update(req(
            &t.writer,
            CreateProjectStatusUpdateRequest {
                project_id: p.id.clone(),
                status: ProjectStatus::OnTrack as i32,
                body: "going well".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let u2 = s
        .create_project_status_update(req(
            &t.writer,
            CreateProjectStatusUpdateRequest {
                project_id: p.id.clone(),
                status: ProjectStatus::AtRisk as i32,
                body: "slipping".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let proj = s
        .get_project(req(
            &t.reader,
            GetProjectRequest {
                project_id: p.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(proj.latest_status_update.unwrap().id, u2.id);
    let ups = s
        .list_project_status_updates(req(
            &t.reader,
            ListProjectStatusUpdatesRequest {
                project_id: p.id.clone(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .status_updates;
    assert_eq!(
        ups.iter().map(|u| u.id.clone()).collect::<Vec<_>>(),
        vec![u2.id.clone(), u1.id.clone()]
    );
    let edited = s
        .update_project_status_update(req(
            &t.writer,
            UpdateProjectStatusUpdateRequest {
                project_id: p.id.clone(),
                status_update_id: u2.id.clone(),
                status: ProjectStatus::OffTrack as i32,
                body: Some("late".into()),
                expected_version: 1,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (edited.status, edited.version),
        (ProjectStatus::OffTrack as i32, 2)
    );
    s.delete_project_status_update(req(
        &t.writer,
        DeleteProjectStatusUpdateRequest {
            project_id: p.id.clone(),
            status_update_id: u2.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        s.get_project(req(
            &t.reader,
            GetProjectRequest {
                project_id: p.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .latest_status_update
        .unwrap()
        .id,
        u1.id
    );
    // Field edits clear values of removed options; deleting a field drops values and view refs.
    s.update_project_field(req(
        &t.writer,
        UpdateProjectFieldRequest {
            project_id: p.id.clone(),
            field_id: status.id.clone(),
            options: vec![status.options[0].clone(), status.options[2].clone()],
            set_options: true,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        items(&w, &t.reader, &p.id, "status:\"In Progress\"")
            .await
            .len(),
        0
    );
    let by_effort = s
        .create_project_view(req(
            &t.writer,
            CreateProjectViewRequest {
                project_id: p.id.clone(),
                name: "E".into(),
                sort_by: vec![ProjectViewSort {
                    field_id: effort.id.clone(),
                    descending: false,
                }],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    s.delete_project_field(req(
        &t.writer,
        DeleteProjectFieldRequest {
            project_id: p.id.clone(),
            field_id: effort.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert!(
        s.get_project_view(req(
            &t.reader,
            GetProjectViewRequest {
                project_id: p.id.clone(),
                view_id: by_effort.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .sort_by
        .is_empty()
    );
    assert_eq!(
        reason(
            s.delete_project_field(req(
                &t.writer,
                DeleteProjectFieldRequest {
                    project_id: p.id.clone(),
                    field_id: fs[0].id.clone()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    // Repo and team links.
    let tools = s
        .link_project_repo(req(
            &t.admin,
            LinkProjectRepoRequest {
                project_id: p.id.clone(),
                repo: rref("acme/tools"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(tools.linked_repo_ids.len(), 2);
    let unl = s
        .unlink_project_repo(req(
            &t.admin,
            UnlinkProjectRepoRequest {
                project_id: p.id.clone(),
                repo: rref("acme/tools"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(unl.linked_repo_ids.len(), 1);
    let team = s
        .link_project_team(req(
            &t.admin,
            LinkProjectTeamRequest {
                project_id: p.id.clone(),
                team: "core".into(),
                role: ProjectTeamRole::Write as i32,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(team.linked_teams[0].role, ProjectTeamRole::Write as i32);
    assert!(
        s.unlink_project_team(req(
            &t.admin,
            UnlinkProjectTeamRequest {
                project_id: p.id.clone(),
                team: "core".into()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .linked_teams
        .is_empty()
    );
}

// ------------------------------------------- AS-ISSUE-01: triage ten issues

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_issue_01_triage_ten_issues_into_types_labels_sub_issues_and_an_iteration() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    // Ten reports arrive from people with no triage rights.
    let titles = [
        "Login fails with SSO",
        "Typo in README",
        "Add dark mode",
        "Crash when uploading 2GB file",
        "Docs: explain retries",
        "Slow dashboard",
        "Support CSV export",
        "Broken link in footer",
        "Rate limit headers missing",
        "Upgrade to TLS 1.3",
    ];
    let mut issues = Vec::new();
    for (n, title) in titles.iter().enumerate() {
        let who = if n % 2 == 0 {
            &t.reader
        } else {
            &t.outsider_member(&w)
        };
        issues.push(open(&w, who, title).await);
    }
    assert_eq!(
        issues.iter().map(|i| i.number).collect::<Vec<_>>(),
        (1..=10).collect::<Vec<u64>>()
    );
    // The triager's tools: types, a priority field, a project with a sprint.
    let mk_type = |name: &str| {
        s.create_issue_type(req(
            &t.admin,
            CreateIssueTypeRequest {
                org: "acme".into(),
                name: name.into(),
                enabled: true,
                ..Default::default()
            },
        ))
    };
    for n in ["Bug", "Feature", "Task"] {
        mk_type(n).await.unwrap();
    }
    let prio = s
        .create_issue_field(req(
            &t.admin,
            CreateIssueFieldRequest {
                org: "acme".into(),
                name: "Priority".into(),
                kind: IssueFieldKind::SingleSelect as i32,
                options: ["Urgent", "High", "Low"]
                    .iter()
                    .map(|n| IssueFieldOption {
                        name: n.to_string(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let project = make_project(&w, &t.writer, "Triage board", false).await;
    let fs = fields_of(&w, &t.writer, &project.id).await;
    let status = field(&fs, "Status").clone();
    let sprint = s
        .create_project_field(req(
            &t.writer,
            CreateProjectFieldRequest {
                project_id: project.id.clone(),
                name: "Sprint".into(),
                kind: ProjectFieldKind::Iteration as i32,
                iteration_config: Some(ProjectIterationConfig {
                    start_on: Some(pbjson_types::Timestamp {
                        seconds: (w.env.clock.now_ms() / 1000) as i64 - 86_400,
                        nanos: 0,
                    }),
                    duration_days: 14,
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let current = s
        .add_project_iteration(req(
            &t.writer,
            AddProjectIterationRequest {
                project_id: project.id.clone(),
                field_id: sprint.id.clone(),
                title: "Sprint 1".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    s.add_project_iteration(req(
        &t.writer,
        AddProjectIterationRequest {
            project_id: project.id.clone(),
            field_id: sprint.id.clone(),
            title: "Sprint 2".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    // Classification by title.
    let plan: [(&str, &str, &str); 10] = [
        ("Bug", "bug", "Urgent"),
        ("Task", "documentation", "Low"),
        ("Feature", "enhancement", "Low"),
        ("Bug", "bug", "Urgent"),
        ("Task", "documentation", "Low"),
        ("Bug", "bug", "High"),
        ("Feature", "enhancement", "High"),
        ("Bug", "bug", "Low"),
        ("Feature", "enhancement", "High"),
        ("Task", "enhancement", "High"),
    ];
    for (i, (ty, label, pr)) in plan.iter().enumerate() {
        let n = (i + 1) as u64;
        s.set_issue_type(req(
            &t.writer,
            SetIssueTypeRequest {
                repo: rref("acme/app"),
                number: n,
                issue_type: ty.to_string(),
            },
        ))
        .await
        .unwrap();
        s.add_issue_labels(req(
            &t.writer,
            AddIssueLabelsRequest {
                repo: rref("acme/app"),
                number: n,
                labels: vec![label.to_string()],
                create_missing: false,
            },
        ))
        .await
        .unwrap();
        s.set_issue_field_values(req(
            &t.writer,
            SetIssueFieldValuesRequest {
                repo: rref("acme/app"),
                number: n,
                values: vec![IssueFieldValue {
                    field_id: prio.id.clone(),
                    value: Some(issue_field_value::Value::OptionId(pr.to_string())),
                    ..Default::default()
                }],
            },
        ))
        .await
        .unwrap();
    }
    // Group: #4 and #6 and #8 are parts of #1 (an epic); #7 and #9 are parts of #3.
    for (parent, kids) in [(1u64, vec![4u64, 6, 8]), (3, vec![7, 9])] {
        for k in kids {
            s.add_issue_sub_issue(req(
                &t.writer,
                AddIssueSubIssueRequest {
                    repo: rref("acme/app"),
                    number: parent,
                    sub_issue: iref("acme/app", k),
                    replace_parent: false,
                    after_id: String::new(),
                },
            ))
            .await
            .unwrap();
        }
    }
    // Into the project and the current sprint; urgent ones start immediately.
    for (i, issue) in issues.iter().enumerate() {
        let item = add_issue_item(&w, &t.writer, &project.id, issue).await;
        s.set_project_item_field_value(req(
            &t.writer,
            SetProjectItemFieldValueRequest {
                project_id: project.id.clone(),
                item_id: item.id.clone(),
                value: Some(ProjectFieldValue {
                    field_id: sprint.id.clone(),
                    value: Some(project_field_value::Value::IterationId(if i < 6 {
                        current.id.clone()
                    } else {
                        "Sprint 2".into()
                    })),
                    ..Default::default()
                }),
                expected_version: 0,
            },
        ))
        .await
        .unwrap();
        if plan[i].2 == "Urgent" {
            s.set_project_item_field_value(req(
                &t.writer,
                SetProjectItemFieldValueRequest {
                    project_id: project.id.clone(),
                    item_id: item.id.clone(),
                    value: Some(ProjectFieldValue {
                        field_id: status.id.clone(),
                        value: Some(project_field_value::Value::OptionId("In Progress".into())),
                        ..Default::default()
                    }),
                    expected_version: 0,
                },
            ))
            .await
            .unwrap();
        }
    }
    // The result, read back through filters and relations.
    assert_eq!(list(&w, &t.reader, "label:bug").await, vec![1, 4, 6, 8]);
    assert_eq!(
        list(&w, &t.reader, "label:enhancement").await,
        vec![3, 7, 9, 10]
    );
    let features = s
        .list_issues(req(
            &t.reader,
            ListIssuesRequest {
                repo: rref("acme/app"),
                issue_type: "Feature".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        features
            .issues
            .iter()
            .map(|i| i.number)
            .collect::<Vec<_>>()
            .len(),
        3
    );
    assert_eq!(list(&w, &t.reader, "no:label").await, Vec::<u64>::new());
    let epic = get(&w, &t.reader, 1).await;
    assert_eq!(epic.sub_issues.unwrap().total, 3);
    assert_eq!(get(&w, &t.reader, 7).await.parent.unwrap().number, 3);
    assert_eq!(
        get(&w, &t.reader, 4).await.field_values[0].field_name,
        "Priority"
    );
    assert_eq!(
        items(&w, &t.reader, &project.id, "sprint:@current")
            .await
            .len(),
        6
    );
    assert_eq!(
        items(
            &w,
            &t.reader,
            &project.id,
            "sprint:\"Sprint 2\" label:enhancement"
        )
        .await
        .len(),
        3
    );
    assert_eq!(
        items(&w, &t.reader, &project.id, "status:\"In Progress\"").await,
        vec!["Login fails with SSO", "Crash when uploading 2GB file"]
    );
    assert_eq!(
        items(&w, &t.reader, &project.id, "type:Bug status:Todo").await,
        vec!["Slow dashboard", "Broken link in footer"]
    );
    assert_eq!(
        items(&w, &t.reader, &project.id, "has:parent-issue")
            .await
            .len(),
        5
    );
    assert_eq!(
        items(&w, &t.reader, &project.id, "no:parent-issue")
            .await
            .len(),
        5
    );
    // Everything triage did is on the timeline and in the audit log.
    let tl = kinds(&timeline(&w, 4).await);
    for k in [
        IssueTimelineEventKind::TypeChanged,
        IssueTimelineEventKind::Labeled,
        IssueTimelineEventKind::FieldChanged,
        IssueTimelineEventKind::ParentAdded,
        IssueTimelineEventKind::AddedToProject,
        IssueTimelineEventKind::ProjectStatusChanged,
    ] {
        assert!(tl.contains(&k), "{k:?} missing from {tl:?}");
    }
    let audit = audit_entries(&w).await;
    for a in [
        "issue.create",
        "issue.set_type",
        "issue.label",
        "issue.set_field_values",
        "issue.add_sub_issue",
        "project.create",
        "project_item.add",
        "project_item.set_value",
    ] {
        assert!(
            audit.iter().any(|(x, r)| x == a && r == "ok"),
            "no audit entry {a}"
        );
    }
    let ev = event_kinds(&w).await;
    for k in [
        "issue.opened",
        "issue.typed",
        "issue.labeled",
        "issue.field_changed",
        "issue.sub_issue_changed",
    ] {
        assert!(ev.iter().any(|x| x == k), "no event {k}");
    }
}

impl Toks {
    /// A second human who is an org member without triage rights.
    fn outsider_member(&self, w: &W) -> String {
        w.env.full("reader")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinning_is_capped_at_three_and_ordered() {
    let w = world().await;
    let (s, t) = (&w.s, &w.t);
    for n in 1..=4 {
        open(&w, &t.writer, &format!("pin {n}")).await;
    }
    let pin = |n: u64| {
        s.pin_issue(req(
            &t.writer,
            PinIssueRequest {
                repo: rref("acme/app"),
                number: n,
            },
        ))
    };
    for n in [3, 1, 2] {
        assert!(pin(n).await.unwrap().into_inner().pinned);
    }
    assert_eq!(reason(pin(4).await), "RESOURCE_EXHAUSTED");
    let listed = s
        .list_pinned_issues(req(
            &t.reader,
            ListPinnedIssuesRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        listed.issues.iter().map(|i| i.number).collect::<Vec<_>>(),
        vec![3, 1, 2]
    );
    s.unpin_issue(req(
        &t.writer,
        UnpinIssueRequest {
            repo: rref("acme/app"),
            number: 3,
        },
    ))
    .await
    .unwrap();
    assert!(pin(4).await.is_ok());
    assert!(!get(&w, &t.reader, 3).await.pinned);
}
