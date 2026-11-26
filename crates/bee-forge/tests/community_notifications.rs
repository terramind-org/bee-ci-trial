//! Notifications: fan-out from forge events into per-principal inboxes, the inbox and
//! subscription calls, repo watching, email deliveries with reply tokens, scheduled reminders
//! and the live inbox stream (AS-COLLAB-02).

use crate::community_support as support;

use std::time::Duration;

use bee_auth::Scope;
use bee_forge::Cx;
use bee_forge::community::fanout::derive;
use bee_forge::community::notifications::{Filter, Mark, Selector, delivery_state};
use bee_proto::v1::discussion_service_server::DiscussionService;
use bee_proto::v1::notification_service_server::NotificationService;
use bee_proto::v1::release_service_server::ReleaseService;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::{event, *};
use support::*;
use tokio_stream::StreamExt;

fn ts_at(ms: u64) -> Option<pbjson_types::Timestamp> {
    Some(bee_forge::ids::ts(ms))
}

fn change(repo: &Repo, n: u64, author: &str, reviewers: &[&str], draft: bool) -> ChangeRequest {
    ChangeRequest {
        id: format!("cr-{n}"),
        repo_id: repo.id.clone(),
        number: n,
        title: format!("Change {n}"),
        state: ChangeState::Open as i32,
        draft,
        author: Some(human(author)),
        review_requests: reviewers
            .iter()
            .map(|r| ReviewRequest {
                reviewer: Some(human(r)),
                requested_by: Some(human(author)),
                requested_at: ts_at(T0),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn cr_event(kind: &str, repo: &Repo, cr: ChangeRequest, actor: &str) -> Event {
    Event {
        kind: kind.into(),
        repo_id: repo.id.clone(),
        org_id: repo.org_id.clone(),
        principal: actor.into(),
        subject: Some(SubjectRef {
            kind: SubjectKind::ChangeRequest as i32,
            repo: rid(&repo.id),
            id: cr.id.clone(),
        }),
        body: Some(event::Body::Change(cr)),
        ..Default::default()
    }
}

fn issue_event(
    kind: &str,
    repo: &Repo,
    number: u64,
    author: &str,
    comment: Option<(&str, &str, &str)>,
    assignees: &[&str],
) -> Event {
    let issue = serde_json::json!({
        "id": format!("iss-{number}"),
        "number": number.to_string(),
        "title": format!("Issue {number}"),
        "body": "something broke",
        "author": {"id": author},
        "assignees": assignees.iter().map(|a| serde_json::json!({"id": a})).collect::<Vec<_>>(),
    });
    let mut payload = serde_json::json!({ "issue": issue });
    let actor = match comment {
        Some((id, who, body)) => {
            payload["comment"] = serde_json::json!({"id": id, "body": body, "author": {"id": who}});
            who
        }
        None => author,
    };
    Event {
        kind: kind.into(),
        repo_id: repo.id.clone(),
        org_id: repo.org_id.clone(),
        principal: actor.into(),
        subject: Some(SubjectRef {
            kind: SubjectKind::Issue as i32,
            repo: rid(&repo.id),
            id: format!("iss-{number}"),
        }),
        payload_json: payload.to_string(),
        ..Default::default()
    }
}

async fn publish(w: &World, e: Event, key: &str) {
    w.env
        .forge
        .events
        .append(&Cx::system(), e, key)
        .await
        .unwrap();
    // Distinct times keep inbox ordering meaningful.
    w.clock.advance(10);
}

async fn titles(w: &World, who: &str) -> Vec<String> {
    inbox(w, who, Default::default())
        .await
        .into_iter()
        .map(|t| t.title)
        .collect()
}

async fn inbox(w: &World, who: &str, f: NotificationThreadFilter) -> Vec<NotificationThread> {
    w.s.list_notification_threads(req(
        &w.tok(who),
        ListNotificationThreadsRequest {
            filter: Some(f),
            ..Default::default()
        },
    ))
    .await
    .unwrap()
    .into_inner()
    .threads
}

fn ids(t: &[NotificationThread]) -> Vec<String> {
    t.iter().map(|t| t.id.clone()).collect()
}

fn by_ids(ids: &[&str]) -> Option<NotificationThreadSelector> {
    Some(NotificationThreadSelector {
        thread_ids: ids.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    })
}

fn thread_key(kind: SubjectKind, repo: &Repo, id: &str) -> String {
    bee_forge::community::notifications::subject_key(kind as i32, &repo.id, id)
}

async fn watch(
    w: &World,
    who: &str,
    repo: &str,
    level: RepoWatchLevel,
    custom: &[RepoWatchEventKind],
) -> Result<RepoWatch, String> {
    w.s.set_repo_watch(req(
        &w.tok(who),
        SetRepoWatchRequest {
            repo: rref(repo),
            level: level as i32,
            custom_events: custom.iter().map(|k| *k as i32).collect(),
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|st| reason::<()>(Err(st)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_collab_02_work_through_the_notification_inbox() {
    let w = world().await;
    let agent = w.env.agent_token("writer", Scope::new());
    // Review request, assignment and a mention arrive from other domains' events.
    publish(
        &w,
        cr_event(
            "cr.opened",
            &w.app,
            change(&w.app, 1, "writer", &["agent"], false),
            "writer",
        ),
        "e1",
    )
    .await;
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            12,
            "reader",
            Some(("c1", "reader", "can you look, @agent-login?")),
            &[],
        ),
        "e2",
    )
    .await;
    publish(
        &w,
        issue_event("issue.opened", &w.app, 13, "reader", None, &["agent"]),
        "e3",
    )
    .await;
    w.flush().await;

    let count = w
        .s
        .get_notification_unread_count(req(&agent, GetNotificationUnreadCountRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(count.total, 3);
    assert_eq!(count.by_reason["NOTIFICATION_REASON_REVIEW_REQUESTED"], 1);
    assert_eq!(count.by_reason["NOTIFICATION_REASON_MENTION"], 1);
    assert_eq!(count.by_reason["NOTIFICATION_REASON_ASSIGN"], 1);
    assert_eq!(count.by_repo["acme/app"], 3);

    // Review requests first.
    let review = inbox(
        &w,
        "agent",
        NotificationThreadFilter {
            reasons: vec![NotificationReason::ReviewRequested as i32],
            ..Default::default()
        },
    )
    .await;
    assert_eq!(review.len(), 1);
    let t = &review[0];
    assert_eq!(t.id, thread_key(SubjectKind::ChangeRequest, &w.app, "cr-1"));
    assert_eq!(
        (
            t.title.as_str(),
            t.last_activity.as_str(),
            t.subject_state.as_str()
        ),
        ("Change 1", "requested your review", "open")
    );
    assert_eq!(t.last_actor.as_ref().unwrap().id, "writer");
    assert_eq!(t.repo_full_name, "acme/app");
    assert!(t.unread && !t.saved && !t.done && t.url.ends_with("/acme/app/changes/1"));
    // The author of the change request is not told about their own request.
    assert!(inbox(&w, "writer", Default::default()).await.is_empty());
    // Read it, then act on the rest: mark the mention done, save the assignment.
    let got =
        w.s.get_notification_thread(req(
            &agent,
            GetNotificationThreadRequest {
                thread_id: t.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.id, t.id);
    let read =
        w.s.mark_notification_threads_read(req(
            &agent,
            MarkNotificationThreadsReadRequest {
                selector: by_ids(&[&t.id]),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.updated_count, 1);
    assert!(
        !w.s.get_notification_thread(req(
            &agent,
            GetNotificationThreadRequest {
                thread_id: t.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .unread
    );
    // Marking again changes nothing.
    assert_eq!(
        w.s.mark_notification_threads_read(req(
            &agent,
            MarkNotificationThreadsReadRequest {
                selector: by_ids(&[&t.id])
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .updated_count,
        0
    );
    let mention = thread_key(SubjectKind::Issue, &w.app, "iss-12");
    let assign = thread_key(SubjectKind::Issue, &w.app, "iss-13");
    w.s.mark_notification_threads_done(req(
        &agent,
        MarkNotificationThreadsDoneRequest {
            selector: by_ids(&[&mention]),
        },
    ))
    .await
    .unwrap();
    w.s.save_notification_threads(req(
        &agent,
        SaveNotificationThreadsRequest {
            selector: by_ids(&[&assign]),
        },
    ))
    .await
    .unwrap();
    // Done threads leave the default list but stay findable.
    assert_eq!(ids(&inbox(&w, "agent", Default::default()).await).len(), 2);
    assert_eq!(
        ids(&inbox(
            &w,
            "agent",
            NotificationThreadFilter {
                done_only: true,
                ..Default::default()
            }
        )
        .await),
        vec![mention.clone()]
    );
    assert_eq!(
        ids(&inbox(
            &w,
            "agent",
            NotificationThreadFilter {
                saved_only: true,
                ..Default::default()
            }
        )
        .await),
        vec![assign.clone()]
    );
    assert_eq!(
        ids(&inbox(
            &w,
            "agent",
            NotificationThreadFilter {
                unread_only: true,
                ..Default::default()
            }
        )
        .await),
        vec![assign.clone()]
    );
    assert_eq!(
        w.s.get_notification_unread_count(req(
            &agent,
            GetNotificationUnreadCountRequest::default()
        ))
        .await
        .unwrap()
        .into_inner()
        .total,
        1
    );
    // Bulk by filter needs a `before` horizon.
    let bulk = |before: Option<pbjson_types::Timestamp>| {
        w.s.mark_notification_threads_read(req(
            &agent,
            MarkNotificationThreadsReadRequest {
                selector: Some(NotificationThreadSelector {
                    filter: Some(NotificationThreadFilter {
                        unread_only: true,
                        ..Default::default()
                    }),
                    before,
                    ..Default::default()
                }),
            },
        ))
    };
    assert_eq!(reason(bulk(None).await), "INVALID_ARGUMENT");
    assert_eq!(
        bulk(ts_at(T0 + 10_000))
            .await
            .unwrap()
            .into_inner()
            .updated_count,
        1
    );
    // New activity on a done thread brings it back, unread.
    w.clock.advance(1000);
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            12,
            "reader",
            Some(("c2", "reader", "any news @agent-login?")),
            &[],
        ),
        "e4",
    )
    .await;
    w.flush().await;
    let back = inbox(
        &w,
        "agent",
        NotificationThreadFilter {
            unread_only: true,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(ids(&back), vec![mention.clone()]);
    assert_eq!(
        (
            back[0].latest_comment_id.as_str(),
            back[0].last_activity.as_str()
        ),
        ("c2", "mentioned you")
    );
    assert!(!back[0].done);
    // Unknown threads and other people's threads are NOT_FOUND.
    assert_eq!(
        reason(
            w.s.get_notification_thread(req(
                &w.tok("reader"),
                GetNotificationThreadRequest {
                    thread_id: t.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(
            w.s.mark_notification_threads_done(req(
                &agent,
                MarkNotificationThreadsDoneRequest {
                    selector: by_ids(&["nope"])
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // Subscriptions: unsubscribe silences ordinary activity but not a direct mention.
    let subject = Some(SubjectRef {
        kind: SubjectKind::Issue as i32,
        repo: rref("acme/app"),
        id: "iss-13".into(),
    });
    let sub =
        w.s.get_notification_subscription(req(
            &agent,
            GetNotificationSubscriptionRequest {
                subject: subject.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    // Being assigned subscribed the agent implicitly.
    assert_eq!(
        (sub.state, sub.reason),
        (
            NotificationSubscriptionState::Subscribed as i32,
            NotificationReason::Assign as i32
        )
    );
    let unsub =
        w.s.unsubscribe_notification_thread(req(
            &agent,
            UnsubscribeNotificationThreadRequest {
                subject: subject.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        unsub.state,
        NotificationSubscriptionState::Unsubscribed as i32
    );
    w.s.mark_notification_threads_read(req(
        &agent,
        MarkNotificationThreadsReadRequest {
            selector: by_ids(&[&assign, &mention]),
        },
    ))
    .await
    .unwrap();
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            13,
            "reader",
            Some(("c3", "reader", "plain comment")),
            &[],
        ),
        "e5",
    )
    .await;
    w.flush().await;
    assert!(
        !w.s.get_notification_thread(req(
            &agent,
            GetNotificationThreadRequest {
                thread_id: assign.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .unread
    );
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            13,
            "reader",
            Some(("c4", "reader", "@agent-login please")),
            &[],
        ),
        "e6",
    )
    .await;
    w.flush().await;
    assert!(
        w.s.get_notification_thread(req(
            &agent,
            GetNotificationThreadRequest {
                thread_id: assign.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .unread
    );
    // Subscribe again; ignore.
    assert_eq!(
        w.s.subscribe_notification_thread(req(
            &agent,
            SubscribeNotificationThreadRequest {
                subject: subject.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .reason,
        NotificationReason::Manual as i32
    );
    assert_eq!(
        w.s.ignore_notification_thread(req(&agent, IgnoreNotificationThreadRequest { subject }))
            .await
            .unwrap()
            .into_inner()
            .state,
        NotificationSubscriptionState::Ignored as i32
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fan_out_respects_watching_subscriptions_access_and_own_activity() {
    let w = world().await;
    // Watching needs read access: a stranger cannot even see the private repo.
    assert_eq!(
        watch(&w, "outsider", "acme/app", RepoWatchLevel::All, &[])
            .await
            .unwrap_err(),
        "NOT_FOUND"
    );
    assert_eq!(
        watch(&w, "reader", "acme/app", RepoWatchLevel::Custom, &[])
            .await
            .unwrap_err(),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        watch(
            &w,
            "reader",
            "acme/app",
            RepoWatchLevel::All,
            &[RepoWatchEventKind::Issues]
        )
        .await
        .unwrap_err(),
        "INVALID_ARGUMENT"
    );
    watch(&w, "reader", "acme/app", RepoWatchLevel::All, &[])
        .await
        .unwrap();
    watch(
        &w,
        "triager",
        "acme/app",
        RepoWatchLevel::Custom,
        &[RepoWatchEventKind::Issues],
    )
    .await
    .unwrap();
    watch(&w, "ci", "acme/app", RepoWatchLevel::Ignore, &[])
        .await
        .unwrap();
    watch(&w, "admin", "acme/app", RepoWatchLevel::Participating, &[])
        .await
        .unwrap();
    // A watch row that outlived the watcher's access (forced through the store) is ignored.
    w.env
        .forge
        .community
        .notifications
        .set_watch(&w.cx("outsider"), &w.app.id, RepoWatchLevel::All, &[])
        .await
        .unwrap();
    let watchers =
        w.s.list_repo_watchers(req(
            &w.tok("reader"),
            ListRepoWatchersRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(watchers.total_count, 4); // everyone but the ignorer
    let mine =
        w.s.list_watched_repos(req(&w.tok("triager"), ListWatchedReposRequest::default()))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(mine.watches.len(), 1);
    assert_eq!(
        mine.watches[0].custom_events,
        vec![RepoWatchEventKind::Issues as i32]
    );
    assert_eq!(
        w.s.get_repo_watch(req(
            &w.tok("admin"),
            GetRepoWatchRequest {
                repo: rref("acme/app")
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .level,
        RepoWatchLevel::Participating as i32
    );
    assert_eq!(
        w.s.get_repo_watch(req(
            &w.tok("writer"),
            GetRepoWatchRequest {
                repo: rref("acme/app")
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .level,
        RepoWatchLevel::Unspecified as i32
    );

    // A change request: `all` hears it, `custom: issues` does not, `ignore` and the
    // stranger do not, and the author is not told about their own change.
    publish(
        &w,
        cr_event(
            "cr.opened",
            &w.app,
            change(&w.app, 1, "writer", &[], false),
            "writer",
        ),
        "a",
    )
    .await;
    // An issue: `custom: issues` hears it too.
    publish(
        &w,
        issue_event("issue.opened", &w.app, 5, "writer", None, &[]),
        "b",
    )
    .await;
    w.flush().await;
    let reader = inbox(&w, "reader", Default::default()).await;
    assert_eq!(
        reader.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        vec!["Issue 5", "Change 1"]
    );
    assert!(
        reader
            .iter()
            .all(|t| t.reasons == vec![NotificationReason::Subscribed as i32]
                && t.reason == NotificationReason::Subscribed as i32)
    );
    assert_eq!(titles(&w, "triager").await, vec!["Issue 5"]);
    for who in ["ci", "outsider", "writer", "admin"] {
        assert!(
            titles(&w, who).await.is_empty(),
            "{who} should have nothing"
        );
    }
    // Commenting makes you a participant: admin's own comment does not notify them, but the
    // issue's author hears it, and admin hears the reply.
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            5,
            "writer",
            Some(("c1", "admin", "I will fix it")),
            &[],
        ),
        "c",
    )
    .await;
    w.flush().await;
    assert!(titles(&w, "admin").await.is_empty());
    assert_eq!(
        inbox(&w, "writer", Default::default()).await[0].reason,
        NotificationReason::Author as i32
    );
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            5,
            "writer",
            Some(("c2", "writer", "thanks admin")),
            &[],
        ),
        "d",
    )
    .await;
    w.flush().await;
    let admin = inbox(&w, "admin", Default::default()).await;
    assert_eq!(
        (admin.len(), admin[0].reason),
        (1, NotificationReason::Comment as i32)
    );
    // Own activity is off by default and can be switched on: a done thread stays done after
    // the writer's own comment, then comes back once they opt in.
    let key = thread_key(SubjectKind::Issue, &w.app, "iss-5");
    w.s.mark_notification_threads_done(req(
        &w.tok("writer"),
        MarkNotificationThreadsDoneRequest {
            selector: by_ids(&[&key]),
        },
    ))
    .await
    .unwrap();
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            5,
            "writer",
            Some(("c3", "writer", "one more")),
            &[],
        ),
        "e",
    )
    .await;
    w.flush().await;
    assert!(titles(&w, "writer").await.is_empty());
    w.s.update_notification_settings(req(
        &w.tok("writer"),
        UpdateNotificationSettingsRequest {
            include_own_activity: Some(true),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            5,
            "writer",
            Some(("c3b", "writer", "and another")),
            &[],
        ),
        "e2",
    )
    .await;
    w.flush().await;
    assert_eq!(titles(&w, "writer").await, vec!["Issue 5"]);
    // The ignorer is silent even when subscribed by hand, yet a direct mention still reaches them.
    let subj = Some(SubjectRef {
        kind: SubjectKind::Issue as i32,
        repo: rref("acme/app"),
        id: "iss-5".into(),
    });
    w.s.subscribe_notification_thread(req(
        &w.tok("ci"),
        SubscribeNotificationThreadRequest { subject: subj },
    ))
    .await
    .unwrap();
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            5,
            "writer",
            Some(("c4", "writer", "plain")),
            &[],
        ),
        "f",
    )
    .await;
    w.flush().await;
    assert!(titles(&w, "ci").await.is_empty());
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            5,
            "writer",
            Some(("c5", "writer", "@ci-login look")),
            &[],
        ),
        "g",
    )
    .await;
    w.flush().await;
    assert_eq!(titles(&w, "ci").await, vec!["Issue 5"]);
    // Stopping the watch removes it.
    w.s.delete_repo_watch(req(
        &w.tok("reader"),
        DeleteRepoWatchRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        w.s.get_repo_watch(req(
            &w.tok("reader"),
            GetRepoWatchRequest {
                repo: rref("acme/app")
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .level,
        RepoWatchLevel::Unspecified as i32
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processing_is_idempotent_and_bounded() {
    let w = world().await;
    watch(&w, "reader", "acme/app", RepoWatchLevel::All, &[])
        .await
        .unwrap();
    for i in 0..40u64 {
        publish(
            &w,
            issue_event("issue.opened", &w.app, 100 + i, "writer", None, &[]),
            &format!("k{i}"),
        )
        .await;
    }
    // Same events again under the same dedupe keys: the log ignores them.
    publish(
        &w,
        issue_event("issue.opened", &w.app, 100, "writer", None, &[]),
        "k0",
    )
    .await;
    w.flush().await;
    w.flush().await;
    let all = inbox(&w, "reader", Default::default()).await;
    assert_eq!(all.len(), 40);
    // Processing one event twice by hand updates, never duplicates, the thread.
    let n = &w.env.forge.community.notifications;
    let e = issue_event(
        "issue_comment.created",
        &w.app,
        100,
        "writer",
        Some(("c1", "writer", "again")),
        &[],
    );
    n.process_event(&Cx::system(), &e, None).await.unwrap();
    n.process_event(&Cx::system(), &e, None).await.unwrap();
    assert_eq!(inbox(&w, "reader", Default::default()).await.len(), 40);
    // Nothing is left queued.
    assert_eq!(n.process_jobs(&Cx::system()).await.unwrap(), 0);
    // Events nobody can be told about leave no jobs behind either.
    publish(
        &w,
        Event {
            kind: "lease.acquired".into(),
            repo_id: w.app.id.clone(),
            org_id: w.app.org_id.clone(),
            ..Default::default()
        },
        "lease",
    )
    .await;
    w.flush().await;
    assert_eq!(n.process_jobs(&Cx::system()).await.unwrap(), 0);
    assert!(
        derive(&Event {
            kind: "reaction.added".into(),
            ..Default::default()
        })
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discussions_and_releases_notify_through_real_events() {
    let w = world().await;
    w.enable_discussions(&w.app).await;
    w.vcs
        .seed(&w.vr(&w.app), &[("a", "1\n")], &human("writer"), "init", T0)
        .await
        .unwrap();
    w.s.create_discussion_category(req(
        &w.tok("admin"),
        CreateDiscussionCategoryRequest {
            repo: rref("acme/app"),
            name: "Q&A".into(),
            format: DiscussionCategoryFormat::QuestionAnswer as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    watch(
        &w,
        "triager",
        "acme/app",
        RepoWatchLevel::Custom,
        &[
            RepoWatchEventKind::Discussions,
            RepoWatchEventKind::Releases,
        ],
    )
    .await
    .unwrap();
    let q =
        w.s.create_discussion(req(
            &w.tok("reader"),
            CreateDiscussionRequest {
                repo: rref("acme/app"),
                category_id: "q-a".into(),
                title: "How?".into(),
                body: "cc @writer-login".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    w.flush().await;
    // The mentioned user and the custom watcher hear about the new discussion.
    let writer = inbox(&w, "writer", Default::default()).await;
    assert_eq!(writer.len(), 1);
    assert_eq!(
        (writer[0].reason, writer[0].last_activity.as_str()),
        (NotificationReason::Mention as i32, "mentioned you")
    );
    assert_eq!(
        writer[0].subject.as_ref().unwrap().kind,
        SubjectKind::Discussion as i32
    );
    assert!(writer[0].url.ends_with("/acme/app/discussions/1"));
    assert_eq!(inbox(&w, "triager", Default::default()).await.len(), 1);
    assert!(inbox(&w, "reader", Default::default()).await.is_empty()); // their own post
    // A comment notifies the author (a subscriber through authorship).
    let c =
        w.s.create_discussion_comment(req(
            &w.tok("writer"),
            CreateDiscussionCommentRequest {
                repo: rref("acme/app"),
                number: q.number,
                body: "Try `bee land`".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    w.flush().await;
    let reader = inbox(&w, "reader", Default::default()).await;
    assert_eq!(reader.len(), 1);
    assert_eq!(
        (reader[0].reason, reader[0].last_activity.as_str()),
        (NotificationReason::Author as i32, "commented")
    );
    // Marking the answer tells the commenter.
    w.s.mark_discussion_answer(req(
        &w.tok("reader"),
        MarkDiscussionAnswerRequest {
            comment_id: c.comment.unwrap().id,
        },
    ))
    .await
    .unwrap();
    w.flush().await;
    let writer = inbox(&w, "writer", Default::default()).await;
    assert_eq!(writer[0].last_activity, "marked an answer");
    assert_eq!(writer[0].subject_state, "answered");
    assert!(
        writer[0]
            .reasons
            .contains(&(NotificationReason::StateChange as i32))
    );
    // A published release reaches watchers of releases.
    w.s.create_release(req(
        &w.tok("writer"),
        CreateReleaseRequest {
            repo: rref("acme/app"),
            tag_name: "v1".into(),
            name: "One".into(),
            target: Some(Revision {
                expr: "main".into(),
            }),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    w.flush().await;
    let rel: Vec<_> = inbox(&w, "triager", Default::default())
        .await
        .into_iter()
        .filter(|t| t.subject.as_ref().unwrap().kind == SubjectKind::Release as i32)
        .collect();
    assert_eq!(rel.len(), 1);
    assert_eq!(
        (rel[0].title.as_str(), rel[0].subject_state.as_str()),
        ("One", "published")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn emails_carry_reply_tokens_and_failed_sends_are_retried_with_backoff() {
    let w = world().await;
    let users = &w.env.forge.community.users;
    // reader verifies an address; the mailer would send the token.
    let added =
        w.s.add_user_email(req(
            &w.tok("reader"),
            AddUserEmailRequest {
                email: "Reader@Example.com".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!added.verified);
    let tokens = users.claim_verifications(&Cx::system(), 10).await.unwrap();
    assert_eq!(tokens.len(), 1);
    users.verify_email(&tokens[0].token).await.unwrap();
    watch(&w, "reader", "acme/open", RepoWatchLevel::All, &[])
        .await
        .unwrap();
    publish(
        &w,
        issue_event("issue.opened", &w.open, 1, "writer", None, &[]),
        "i1",
    )
    .await;
    w.flush().await;
    let n = &w.env.forge.community.notifications;
    let cx = Cx::system();
    let mail = n
        .claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 60_000)
        .await
        .unwrap();
    assert_eq!(mail.len(), 1);
    let m = &mail[0];
    assert_eq!(
        (m.to.as_str(), m.subject.as_str(), m.principal_id.as_str()),
        ("reader@example.com", "[acme/open] Issue 1 (#1)", "reader")
    );
    assert!(
        m.text.contains("writer opened this issue")
            && m.text
                .contains("You are receiving this because you are watching the repository")
    );
    assert!(m.reply_to.starts_with("reply+") && m.reply_to.ends_with("@reply.beehive.invalid"));
    // The token resolves back to the principal and the subject for the inbound mail worker.
    let target = n.resolve_reply(&m.reply_token).await.unwrap();
    assert_eq!(
        (target.principal_id.as_str(), target.thread_id.as_str()),
        (
            "reader",
            thread_key(SubjectKind::Issue, &w.open, "iss-1").as_str()
        )
    );
    assert_eq!(
        n.resolve_reply("deadbeef").await.unwrap_err().reason,
        bee_proto::v1::ErrorReason::NotFound
    );
    // The thread's own reply address is the same token.
    let thread_id = target.thread_id.clone();
    let addr =
        w.s.get_notification_reply_address(req(
            &w.tok("reader"),
            GetNotificationReplyAddressRequest {
                thread_id: thread_id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(addr.token, m.reply_token);
    assert_eq!(addr.address, m.reply_to);
    // Claimed but not completed: invisible until the lease ends.
    assert!(
        n.claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 60_000)
            .await
            .unwrap()
            .is_empty()
    );
    // A failure schedules a retry after the backoff (30 s, then 60 s, ...), until attempts run out.
    n.complete_delivery(&cx, &m.id, Err("smtp 451".into()))
        .await
        .unwrap();
    assert!(
        n.claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 1000)
            .await
            .unwrap()
            .is_empty()
    );
    w.clock.advance(31_000);
    let retry = n
        .claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 1000)
        .await
        .unwrap();
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].attempts, 1);
    n.complete_delivery(&cx, &m.id, Err("smtp 451".into()))
        .await
        .unwrap();
    w.clock.advance(61_000);
    assert_eq!(
        n.claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 1000)
            .await
            .unwrap()
            .len(),
        1
    );
    n.complete_delivery(&cx, &m.id, Ok(())).await.unwrap();
    assert_eq!(
        n.delivery(&cx, &m.id).await.unwrap().state,
        delivery_state::SENT
    );
    w.clock.advance(1_000_000);
    assert!(
        n.claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 1000)
            .await
            .unwrap()
            .is_empty()
    );
    // Too many failures give up for good.
    publish(
        &w,
        issue_event("issue.opened", &w.open, 2, "writer", None, &[]),
        "i2",
    )
    .await;
    w.flush().await;
    let second = n
        .claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 1000)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    for _ in 0..5 {
        n.complete_delivery(&cx, &second[0].id, Err("down".into()))
            .await
            .unwrap();
    }
    let failed = n.delivery(&cx, &second[0].id).await.unwrap();
    assert_eq!(
        (failed.state, failed.attempts, failed.last_error.as_str()),
        (delivery_state::FAILED, 5, "down")
    );
    // Settings: per-reason email off keeps the inbox but sends nothing; replies can be disabled.
    w.s.update_notification_settings(req(
        &w.tok("reader"),
        UpdateNotificationSettingsRequest {
            deliveries: vec![NotificationReasonDelivery {
                reason: NotificationReason::Subscribed as i32,
                web: true,
                email: false,
                mobile: false,
            }],
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    publish(
        &w,
        issue_event("issue.opened", &w.open, 3, "writer", None, &[]),
        "i3",
    )
    .await;
    w.flush().await;
    assert!(
        n.claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 1000)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(inbox(&w, "reader", Default::default()).await.len(), 3);
    // Email addresses in settings must be verified.
    let bad =
        w.s.update_notification_settings(req(
            &w.tok("reader"),
            UpdateNotificationSettingsRequest {
                default_email: Some("stranger@example.com".into()),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(bad), "INVALID_ARGUMENT");
    w.s.update_notification_settings(req(
        &w.tok("reader"),
        UpdateNotificationSettingsRequest {
            reply_by_email: Some(false),
            deliveries: vec![NotificationReasonDelivery {
                reason: NotificationReason::Subscribed as i32,
                web: true,
                email: true,
                mobile: false,
            }],
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.get_notification_reply_address(req(
                &w.tok("reader"),
                GetNotificationReplyAddressRequest {
                    thread_id: thread_id.clone()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    publish(
        &w,
        issue_event("issue.opened", &w.open, 4, "writer", None, &[]),
        "i4",
    )
    .await;
    w.flush().await;
    let plain = n
        .claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 1000)
        .await
        .unwrap();
    assert_eq!(plain.len(), 1);
    assert!(plain[0].reply_to.is_empty());
    // Per-org routing sends mail for acme to the chosen verified address.
    users
        .add_email(&w.cx("reader"), "work@example.com")
        .await
        .unwrap();
    let t2 = users.claim_verifications(&cx, 10).await.unwrap();
    users.verify_email(&t2[0].token).await.unwrap();
    w.s.update_notification_settings(req(
        &w.tok("reader"),
        UpdateNotificationSettingsRequest {
            email_routes: vec![NotificationEmailRoute {
                org: "acme".into(),
                email: "work@example.com".into(),
            }],
            replace_email_routes: true,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    publish(
        &w,
        issue_event("issue.opened", &w.open, 5, "writer", None, &[]),
        "i5",
    )
    .await;
    w.flush().await;
    let routed = n
        .claim_deliveries(&cx, NotificationReminderChannel::Email, 10, 1000)
        .await
        .unwrap();
    assert_eq!(
        routed.iter().map(|d| d.to.as_str()).collect::<Vec<_>>(),
        vec!["work@example.com"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reminders_fire_on_schedule_in_the_owners_time_zone() {
    let w = world().await;
    // Tokens are minted per call: the clock jumps days ahead during this test.
    let tok = || w.env.agent_token("writer", Scope::new());
    // Two review requests wait for the agent; one is a draft.
    publish(
        &w,
        cr_event(
            "cr.opened",
            &w.app,
            change(&w.app, 1, "writer", &["agent"], false),
            "writer",
        ),
        "r1",
    )
    .await;
    publish(
        &w,
        cr_event(
            "cr.opened",
            &w.app,
            change(&w.app, 2, "reader", &["agent"], true),
            "reader",
        ),
        "r2",
    )
    .await;
    w.flush().await;
    let create = |f: &dyn Fn(&mut CreateNotificationReminderRequest)| {
        let mut r = CreateNotificationReminderRequest {
            channel: NotificationReminderChannel::Slack as i32,
            channel_target: "#reviews".into(),
            days: vec![NotificationReminderDay::Monday as i32],
            times: vec!["09:00".into()],
            time_zone: "America/New_York".into(),
            ..Default::default()
        };
        f(&mut r);
        w.s.create_notification_reminder(req(&tok(), r))
    };
    // Validation.
    assert_eq!(
        reason(create(&|r| r.times = vec!["9:00".into()]).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(create(&|r| r.time_zone = "Mars/Olympus".into()).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(create(&|r| r.channel_target.clear()).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(reason(create(&|r| r.channel = 0).await), "INVALID_ARGUMENT");
    assert_eq!(
        reason(
            create(&|r| r.days = vec![
                NotificationReminderDay::Monday as i32,
                NotificationReminderDay::Monday as i32
            ])
            .await
        ),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(create(&|r| r.team_id = "t".into()).await),
        "INVALID_ARGUMENT"
    );
    let rem = create(&|r| {
        r.filter = Some(NotificationReminderFilter {
            ignore_drafts: true,
            ..Default::default()
        })
    })
    .await
    .unwrap()
    .into_inner();
    assert!(rem.enabled && rem.owner.as_ref().unwrap().id == "agent");
    // T0 is Sunday noon UTC; Monday 09:00 in New York (EDT) is 13:00 UTC.
    let n = &w.env.forge.community.notifications;
    let cx = Cx::system();
    w.clock.set(T0 + 24 * 3_600_000 + 59 * 60_000); // Monday 12:59 UTC
    w.flush().await;
    assert!(
        n.claim_deliveries(&cx, NotificationReminderChannel::Slack, 10, 1000)
            .await
            .unwrap()
            .is_empty()
    );
    w.clock.set(T0 + 25 * 3_600_000); // Monday 13:00 UTC
    w.flush().await;
    let sent = n
        .claim_deliveries(&cx, NotificationReminderChannel::Slack, 10, 1000)
        .await
        .unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "#reviews");
    assert!(
        sent[0].text.starts_with("1 item waiting for you:")
            && sent[0].text.contains("Change 1")
            && !sent[0].text.contains("Change 2")
    );
    assert_eq!(sent[0].reminder_id, rem.id);
    // It will not fire twice, and next fires a week later.
    w.flush().await;
    assert!(
        n.claim_deliveries(&cx, NotificationReminderChannel::Slack, 10, 1000)
            .await
            .unwrap()
            .is_empty()
    );
    let after =
        w.s.get_notification_reminder(req(
            &tok(),
            GetNotificationReminderRequest {
                reminder_id: rem.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(after.last_sent_at.is_some());
    n.complete_delivery(&cx, &sent[0].id, Ok(())).await.unwrap();
    w.clock.set(T0 + 25 * 3_600_000 + 7 * 24 * 3_600_000 + 1000);
    w.flush().await;
    let weekly = n
        .claim_deliveries(&cx, NotificationReminderChannel::Slack, 10, 1000)
        .await
        .unwrap();
    assert_eq!(weekly.len(), 1);
    n.complete_delivery(&cx, &weekly[0].id, Ok(()))
        .await
        .unwrap();
    // Disabled reminders stay quiet; deleting removes them.
    w.s.update_notification_reminder(req(
        &tok(),
        UpdateNotificationReminderRequest {
            reminder_id: rem.id.clone(),
            enabled: Some(false),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    w.clock.advance(14 * 24 * 3_600_000);
    w.flush().await;
    assert!(
        n.claim_deliveries(&cx, NotificationReminderChannel::Slack, 10, 1000)
            .await
            .unwrap()
            .is_empty()
    );
    let updated =
        w.s.update_notification_reminder(req(
            &tok(),
            UpdateNotificationReminderRequest {
                reminder_id: rem.id.clone(),
                enabled: Some(true),
                replace_schedule: true,
                times: vec!["08:30".into(), "16:00".into()],
                days: vec![],
                time_zone: Some("Europe/Berlin".into()),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (updated.times.len(), updated.time_zone.as_str()),
        (2, "Europe/Berlin")
    );
    let listed =
        w.s.list_notification_reminders(req(&tok(), ListNotificationRemindersRequest::default()))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(listed.reminders.len(), 1);
    w.s.delete_notification_reminder(req(
        &tok(),
        DeleteNotificationReminderRequest {
            reminder_id: rem.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.get_notification_reminder(req(
                &tok(),
                GetNotificationReminderRequest {
                    reminder_id: rem.id
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // Real-time alerts post at once on a new review request.
    create(&|r| r.real_time_alerts = true).await.unwrap();
    publish(
        &w,
        cr_event(
            "cr.opened",
            &w.app,
            change(&w.app, 3, "writer", &["agent"], false),
            "writer",
        ),
        "r3",
    )
    .await;
    w.flush().await;
    let alert = n
        .claim_deliveries(&cx, NotificationReminderChannel::Slack, 10, 1000)
        .await
        .unwrap();
    assert_eq!(alert.len(), 1);
    assert!(
        alert[0]
            .text
            .contains("writer requested your review: Change 3")
    );
}

type EventStream = std::pin::Pin<
    Box<dyn tokio_stream::Stream<Item = Result<NotificationInboxEvent, tonic::Status>> + Send>,
>;

async fn next_event(s: &mut EventStream) -> NotificationInboxEvent {
    tokio::time::timeout(Duration::from_secs(5), s.next())
        .await
        .expect("stream stalled")
        .expect("stream ended")
        .expect("stream error")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_inbox_stream_replays_a_snapshot_then_follows_live_updates() {
    let w = world().await;
    let agent = w.env.agent_token("writer", Scope::new());
    publish(
        &w,
        cr_event(
            "cr.opened",
            &w.app,
            change(&w.app, 1, "writer", &["agent"], false),
            "writer",
        ),
        "s1",
    )
    .await;
    w.flush().await;
    let mut stream =
        w.s.stream_notification_inbox(req(
            &agent,
            StreamNotificationInboxRequest {
                initial_snapshot: true,
                heartbeat_seconds: 1,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let snap = next_event(&mut stream).await;
    assert_eq!(snap.kind, NotificationInboxEventKind::Snapshot as i32);
    assert_eq!(snap.thread.as_ref().unwrap().title, "Change 1");
    let end = next_event(&mut stream).await;
    assert_eq!(end.kind, NotificationInboxEventKind::SnapshotEnd as i32);
    // Live: a new thread, an update to it, and a removal when it is marked done.
    publish(
        &w,
        issue_event("issue.opened", &w.app, 9, "reader", None, &["agent"]),
        "s2",
    )
    .await;
    w.flush().await;
    let new = next_event(&mut stream).await;
    assert_eq!(new.kind, NotificationInboxEventKind::New as i32);
    assert_eq!(new.thread.as_ref().unwrap().title, "Issue 9");
    let thread_id = new.thread.unwrap().id;
    w.s.mark_notification_threads_done(req(
        &agent,
        MarkNotificationThreadsDoneRequest {
            selector: by_ids(&[&thread_id]),
        },
    ))
    .await
    .unwrap();
    let gone = next_event(&mut stream).await;
    assert_eq!(
        (gone.kind, gone.thread.unwrap().id),
        (NotificationInboxEventKind::Removed as i32, thread_id)
    );
    // Idle streams send heartbeats carrying the cursor.
    let hb = next_event(&mut stream).await;
    assert_eq!(hb.kind, NotificationInboxEventKind::Heartbeat as i32);
    assert!(!hb.cursor.is_empty());
    // Resuming from a cursor sees only what came after.
    publish(
        &w,
        issue_event(
            "issue_comment.created",
            &w.app,
            9,
            "reader",
            Some(("c1", "reader", "@agent-login ping")),
            &[],
        ),
        "s3",
    )
    .await;
    w.flush().await;
    let mut resumed =
        w.s.stream_notification_inbox(req(
            &agent,
            StreamNotificationInboxRequest {
                resume_cursor: hb.cursor.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let ev = next_event(&mut resumed).await;
    assert_eq!(ev.kind, NotificationInboxEventKind::Updated as i32);
    assert_eq!(ev.thread.unwrap().latest_comment_id, "c1");
    let bad =
        w.s.stream_notification_inbox(req(
            &agent,
            StreamNotificationInboxRequest {
                resume_cursor: "junk".into(),
                ..Default::default()
            },
        ))
        .await
        .err()
        .expect("bad cursor is refused");
    assert_eq!(
        bee_proto::errors::detail_from_status(&bad).unwrap().reason,
        "INVALID_ARGUMENT"
    );
    // Filters apply to live events too.
    let mut only_saved =
        w.s.stream_notification_inbox(req(
            &agent,
            StreamNotificationInboxRequest {
                filter: Some(NotificationThreadFilter {
                    saved_only: true,
                    ..Default::default()
                }),
                heartbeat_seconds: 1,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    publish(
        &w,
        issue_event("issue.opened", &w.app, 10, "reader", None, &["agent"]),
        "s4",
    )
    .await;
    w.flush().await;
    assert_eq!(
        next_event(&mut only_saved).await.kind,
        NotificationInboxEventKind::Heartbeat as i32
    );
    // The store-level filter agrees with the proto filter.
    let n = &w.env.forge.community.notifications;
    let (all, _) = n
        .list(&w.cx("agent"), &Filter::default(), "", 50)
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
    let marked = n
        .mark(
            &w.cx("agent"),
            &Selector {
                thread_ids: all.iter().map(|t| t.id.clone()).collect(),
                filter: None,
            },
            Mark::Read,
            0,
        )
        .await
        .unwrap();
    assert_eq!(marked as usize, all.iter().filter(|t| t.unread).count());
}
