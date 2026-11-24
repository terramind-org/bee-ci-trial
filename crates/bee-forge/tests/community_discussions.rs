//! `DiscussionService` through its tonic trait with tokens of different roles: the permission
//! matrix, Q&A answers, polls, upvotes, pins, locks, labels, forms, org discussions and
//! conversion to and from issues.

use crate::community_support as support;

use bee_forge::community::ports::IssueSnapshot;
use bee_proto::v1::discussion_service_server::DiscussionService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::*;
use support::*;

struct D<'a> {
    w: &'a World,
}

impl D<'_> {
    async fn category(
        &self,
        who: &str,
        name: &str,
        format: DiscussionCategoryFormat,
    ) -> Result<DiscussionCategory, String> {
        self.w
            .s
            .create_discussion_category(req(
                &self.w.tok(who),
                CreateDiscussionCategoryRequest {
                    repo: rref("acme/app"),
                    name: name.into(),
                    format: format as i32,
                    ..Default::default()
                },
            ))
            .await
            .map(|r| r.into_inner())
            .map_err(|st| reason::<()>(Err(st)))
    }

    async fn start(&self, who: &str, cat: &str, title: &str) -> Result<Discussion, String> {
        self.w
            .s
            .create_discussion(req(
                &self.w.tok(who),
                CreateDiscussionRequest {
                    repo: rref("acme/app"),
                    category_id: cat.into(),
                    title: title.into(),
                    body: format!("body of {title}"),
                    ..Default::default()
                },
            ))
            .await
            .map(|r| r.into_inner())
            .map_err(|st| reason::<()>(Err(st)))
    }

    async fn upvote(&self, who: &str, add: bool) -> Result<u32, String> {
        let t = self.w.tok(who);
        if add {
            self.w
                .s
                .add_discussion_upvote(req(
                    &t,
                    AddDiscussionUpvoteRequest {
                        repo: rref("acme/app"),
                        number: 1,
                        ..Default::default()
                    },
                ))
                .await
                .map(|r| r.into_inner().upvote_count)
                .map_err(|st| reason::<()>(Err(st)))
        } else {
            self.w
                .s
                .remove_discussion_upvote(req(
                    &t,
                    RemoveDiscussionUpvoteRequest {
                        repo: rref("acme/app"),
                        number: 1,
                        ..Default::default()
                    },
                ))
                .await
                .map(|r| r.into_inner().upvote_count)
                .map_err(|st| reason::<()>(Err(st)))
        }
    }

    async fn comment(
        &self,
        who: &str,
        number: u64,
        body: &str,
        reply_to: &str,
    ) -> Result<DiscussionComment, String> {
        self.w
            .s
            .create_discussion_comment(req(
                &self.w.tok(who),
                CreateDiscussionCommentRequest {
                    repo: rref("acme/app"),
                    number,
                    body: body.into(),
                    reply_to_comment_id: reply_to.into(),
                    ..Default::default()
                },
            ))
            .await
            .map(|r| r.into_inner())
            .map_err(|st| reason::<()>(Err(st)))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn categories_need_discussions_enabled_and_a_maintainer() {
    let w = world().await;
    let d = D { w: &w };
    // Off by default.
    assert_eq!(
        d.category("admin", "General", DiscussionCategoryFormat::OpenEnded)
            .await
            .unwrap_err(),
        "INVALID_STATE"
    );
    w.enable_discussions(&w.app).await;
    // Readers and writers cannot manage categories; strangers cannot even see the repo.
    assert_eq!(
        d.category("reader", "General", DiscussionCategoryFormat::OpenEnded)
            .await
            .unwrap_err(),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        d.category("writer", "General", DiscussionCategoryFormat::OpenEnded)
            .await
            .unwrap_err(),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        d.category("outsider", "General", DiscussionCategoryFormat::OpenEnded)
            .await
            .unwrap_err(),
        "NOT_FOUND"
    );
    let general = d
        .category("admin", "General", DiscussionCategoryFormat::OpenEnded)
        .await
        .unwrap();
    assert_eq!(general.slug, "general");
    assert_eq!(
        d.category("admin", "general", DiscussionCategoryFormat::OpenEnded)
            .await
            .unwrap_err(),
        "ALREADY_EXISTS"
    );
    // Update with a stale version fails; with the current one it works and bumps the version.
    let upd = |v: u64| {
        w.s.update_discussion_category(req(
            &w.tok("admin"),
            UpdateDiscussionCategoryRequest {
                repo: rref("acme/app"),
                category_id: "general".into(),
                emoji: Some(":speech_balloon:".into()),
                expected_version: v,
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(upd(general.version + 5).await), "VERSION_MISMATCH");
    let updated = upd(general.version).await.unwrap().into_inner();
    assert_eq!(updated.emoji, ":speech_balloon:");
    assert!(updated.version > general.version);
    // Readers can list.
    let list =
        w.s.list_discussion_categories(req(
            &w.tok("reader"),
            ListDiscussionCategoriesRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.categories.len(), 1);
    // Deleting a category that holds discussions needs a destination.
    let ideas = d
        .category("admin", "Ideas", DiscussionCategoryFormat::OpenEnded)
        .await
        .unwrap();
    d.start("reader", "ideas", "an idea").await.unwrap();
    let del = |to: &str| {
        w.s.delete_discussion_category(req(
            &w.tok("admin"),
            DeleteDiscussionCategoryRequest {
                repo: rref("acme/app"),
                category_id: ideas.id.clone(),
                move_to_category_id: to.into(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(del("").await), "INVALID_ARGUMENT");
    del("general").await.unwrap();
    let moved =
        w.s.get_discussion_by_number(req(
            &w.tok("reader"),
            GetDiscussionByNumberRequest {
                repo: rref("acme/app"),
                number: 1,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(moved.category.unwrap().slug, "general");
    // Denied state changes are audited.
    w.flush().await;
    let org = w
        .env
        .forge
        .orgs
        .get_org(&w.cx("owner"), "acme")
        .await
        .unwrap();
    let log = w
        .env
        .forge
        .audit
        .query(
            &w.cx("owner"),
            &bee_forge::audit::org_scope(&org.id),
            &AuditLogFilter::default(),
            true,
            "",
            500,
        )
        .await
        .unwrap();
    assert!(
        log.0
            .iter()
            .any(|e| e.action == "discussion.category_create" && e.result == "PERMISSION_DENIED")
    );
    assert!(
        log.0
            .iter()
            .any(|e| e.action == "discussion.category_create"
                && (e.result.is_empty() || e.result == "ok"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn threads_replies_reactions_and_the_discussion_lifecycle() {
    let w = world().await;
    w.enable_discussions(&w.app).await;
    let d = D { w: &w };
    d.category("admin", "General", DiscussionCategoryFormat::OpenEnded)
        .await
        .unwrap();
    let ann = d
        .category(
            "admin",
            "Announcements",
            DiscussionCategoryFormat::Announcement,
        )
        .await
        .unwrap();
    // Anyone who can read can start a discussion; announcements are for maintainers.
    let first = d
        .start("reader", "general", "How do I land?")
        .await
        .unwrap();
    assert_eq!(first.number, 1);
    assert_eq!(first.author_association, "NONE");
    assert_eq!(
        d.start("reader", &ann.id, "hello").await.unwrap_err(),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        d.start("admin", &ann.id, "v2 is out").await.unwrap().number,
        2
    );
    // Threads: a reply to a reply attaches to the root.
    let top = d.comment("writer", 1, "Use `bee land`", "").await.unwrap();
    let top_id = top.comment.unwrap().id;
    let reply1 = d
        .comment("reader", 1, "thanks", &top_id)
        .await
        .unwrap()
        .comment
        .unwrap();
    assert_eq!(reply1.in_reply_to, top_id);
    let reply2 = d
        .comment("writer", 1, "np", &reply1.id)
        .await
        .unwrap()
        .comment
        .unwrap();
    assert_eq!(reply2.in_reply_to, top_id);
    let comments =
        w.s.list_discussion_comments(req(
            &w.tok("reader"),
            ListDiscussionCommentsRequest {
                repo: rref("acme/app"),
                number: 1,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(comments.comments.len(), 1);
    assert_eq!(comments.comments[0].reply_count, 2);
    let replies =
        w.s.list_discussion_comment_replies(req(
            &w.tok("reader"),
            ListDiscussionCommentRepliesRequest {
                comment_id: top_id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(replies.replies.len(), 2);
    assert_eq!(replies.replies[0].comment.as_ref().unwrap().body, "thanks");
    let got =
        w.s.get_discussion(req(
            &w.tok("reader"),
            GetDiscussionRequest {
                id: first.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.comment_count, 3);
    // Reactions on the discussion subject count per viewer.
    w.env
        .forge
        .community
        .reactions
        .add(
            &w.cx("reader"),
            &bee_forge::community::reactions::discussion_key(&first.id),
            &bee_forge::community::reactions::Where {
                org_id: w.app.org_id.clone(),
                repo_id: w.app.id.clone(),
                kind: SubjectKind::Discussion as i32,
                id: first.id.clone(),
            },
            ReactionContent::Heart as i32,
        )
        .await
        .unwrap();
    let got =
        w.s.get_discussion(req(
            &w.tok("reader"),
            GetDiscussionRequest {
                id: first.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.reactions.len(), 1);
    assert!(got.reactions[0].viewer_reacted);
    // Upvotes are idempotent per principal.
    let dd = D { w: &w };
    let up = |who: &'static str, add: bool| dd.upvote(who, add);
    assert_eq!(up("reader", true).await.unwrap(), 1);
    assert_eq!(up("reader", true).await.unwrap(), 1);
    assert_eq!(up("writer", true).await.unwrap(), 2);
    assert_eq!(up("reader", false).await.unwrap(), 1);
    // Comment upvotes count separately.
    let cu =
        w.s.add_discussion_upvote(req(
            &w.tok("reader"),
            AddDiscussionUpvoteRequest {
                repo: rref("acme/app"),
                number: 1,
                comment_id: top_id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cu.upvote_count, 1);
    // Closing: the author or a moderator; other readers cannot.
    let close = |who: &str| {
        w.s.close_discussion(req(
            &w.tok(who),
            CloseDiscussionRequest {
                repo: rref("acme/app"),
                number: 1,
                reason: DiscussionStateReason::Outdated as i32,
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(close("ci").await), "PERMISSION_DENIED");
    let closed = close("reader").await.unwrap().into_inner();
    assert!(closed.closed);
    assert_eq!(closed.state_reason, DiscussionStateReason::Outdated as i32);
    assert_eq!(reason(close("reader").await), "INVALID_STATE");
    // The default listing hides closed discussions.
    let open_list =
        w.s.list_discussions(req(
            &w.tok("reader"),
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        open_list
            .discussions
            .iter()
            .map(|x| x.number)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let all =
        w.s.list_discussions(req(
            &w.tok("reader"),
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                include_closed: true,
                sort: DiscussionSort::Created as i32,
                ascending: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            all.total_count,
            all.discussions.iter().map(|x| x.number).collect::<Vec<_>>()
        ),
        (2, vec![1, 2])
    );
    let reopened =
        w.s.reopen_discussion(req(
            &w.tok("writer"),
            ReopenDiscussionRequest {
                repo: rref("acme/app"),
                number: 1,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!reopened.closed);
    // Edits: the author, or a moderator; a stale version is refused.
    let edit = |who: &str, v: u64| {
        w.s.update_discussion(req(
            &w.tok(who),
            UpdateDiscussionRequest {
                repo: rref("acme/app"),
                number: 1,
                title: Some("How do I land a CR?".into()),
                expected_version: v,
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(edit("ci", 0).await), "PERMISSION_DENIED");
    assert_eq!(reason(edit("reader", 1).await), "VERSION_MISMATCH");
    let edited = edit("reader", reopened.version).await.unwrap().into_inner();
    assert_eq!(edited.title, "How do I land a CR?");
    // Search by qualifier and text.
    let found =
        w.s.list_discussions(req(
            &w.tok("reader"),
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                query: "is:open land".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(found.discussions.len(), 1);
    // Deleting: the author, or a moderator.
    let del = |who: &str| {
        w.s.delete_discussion(req(
            &w.tok(who),
            DeleteDiscussionRequest {
                repo: rref("acme/app"),
                number: 1,
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(del("ci").await), "PERMISSION_DENIED");
    del("reader").await.unwrap();
    assert_eq!(
        reason(
            w.s.get_discussion_by_number(req(
                &w.tok("reader"),
                GetDiscussionByNumberRequest {
                    repo: rref("acme/app"),
                    number: 1,
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // Events and audit entries exist for the changes.
    w.flush().await;
    let kinds = w.event_kinds().await;
    for k in [
        "discussion.created",
        "discussion_comment.created",
        "discussion.closed",
        "discussion.reopened",
        "discussion.edited",
        "discussion.deleted",
        "discussion.upvoted",
    ] {
        assert!(kinds.iter().any(|x| x == k), "missing {k} in {kinds:?}");
    }
    // Archived repos are read-only.
    w.s.archive_repo(req(
        &w.tok("owner"),
        ArchiveRepoRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        d.start("reader", "general", "late").await.unwrap_err(),
        "ARCHIVED"
    );
    assert!(
        w.s.list_discussions(req(
            &w.tok("reader"),
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                ..Default::default()
            }
        ))
        .await
        .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn question_and_answer_categories_accept_one_answer() {
    let w = world().await;
    w.enable_discussions(&w.app).await;
    let d = D { w: &w };
    d.category("admin", "General", DiscussionCategoryFormat::OpenEnded)
        .await
        .unwrap();
    d.category("admin", "Q&A", DiscussionCategoryFormat::QuestionAnswer)
        .await
        .unwrap();
    let q = d
        .start("reader", "q-a", "Why does land fail?")
        .await
        .unwrap();
    let a1 = d
        .comment("writer", q.number, "Rebase first", "")
        .await
        .unwrap()
        .comment
        .unwrap();
    let a2 = d
        .comment("agent", q.number, "Check the leases", "")
        .await
        .unwrap()
        .comment
        .unwrap();
    let mark = |who: &str, id: &str| {
        w.s.mark_discussion_answer(req(
            &w.tok(who),
            MarkDiscussionAnswerRequest {
                comment_id: id.into(),
            },
        ))
    };
    // Neither a bystander nor an unrelated reader can mark; the asker can.
    assert_eq!(reason(mark("ci", &a1.id).await), "PERMISSION_DENIED");
    let answered = mark("reader", &a1.id).await.unwrap().into_inner();
    assert_eq!(answered.answer_comment_id, a1.id);
    assert_eq!(answered.answer_chosen_by.unwrap().id, "reader");
    // Marking another comment moves the answer (a moderator may do it too).
    let moved = mark("triager", &a2.id).await.unwrap().into_inner();
    assert_eq!(moved.answer_comment_id, a2.id);
    let comments =
        w.s.list_discussion_comments(req(
            &w.tok("reader"),
            ListDiscussionCommentsRequest {
                repo: rref("acme/app"),
                number: q.number,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(comments.comments.iter().filter(|c| c.is_answer).count(), 1);
    assert!(
        comments
            .comments
            .iter()
            .find(|c| c.is_answer)
            .unwrap()
            .comment
            .as_ref()
            .unwrap()
            .id
            == a2.id
    );
    // The answered filter.
    let list = |query: &str, answered: Option<bool>| {
        w.s.list_discussions(req(
            &w.tok("reader"),
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                query: query.into(),
                answered,
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        list("is:unanswered", None)
            .await
            .unwrap()
            .into_inner()
            .discussions
            .len(),
        0
    );
    assert_eq!(
        list("", Some(true))
            .await
            .unwrap()
            .into_inner()
            .discussions
            .len(),
        1
    );
    // Unmark.
    let un =
        w.s.unmark_discussion_answer(req(
            &w.tok("reader"),
            UnmarkDiscussionAnswerRequest {
                comment_id: a2.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(un.answer_comment_id.is_empty());
    assert_eq!(
        list("is:unanswered", None)
            .await
            .unwrap()
            .into_inner()
            .discussions
            .len(),
        1
    );
    // Other categories take no answers.
    let g = d.start("reader", "general", "chat").await.unwrap();
    let c = d
        .comment("writer", g.number, "hi", "")
        .await
        .unwrap()
        .comment
        .unwrap();
    assert_eq!(reason(mark("reader", &c.id).await), "INVALID_STATE");
    w.flush().await;
    let kinds = w.event_kinds().await;
    assert!(
        kinds.contains(&"discussion.answered".to_string())
            && kinds.contains(&"discussion.unanswered".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn polls_take_one_changeable_vote_until_closed_or_locked() {
    let w = world().await;
    w.enable_discussions(&w.app).await;
    let d = D { w: &w };
    d.category("admin", "Polls", DiscussionCategoryFormat::Poll)
        .await
        .unwrap();
    // A poll category needs a question and 2 to 8 options.
    assert_eq!(
        d.start("writer", "polls", "which?").await.unwrap_err(),
        "INVALID_ARGUMENT"
    );
    let make = |opts: Vec<&str>| {
        w.s.create_discussion(req(
            &w.tok("writer"),
            CreateDiscussionRequest {
                repo: rref("acme/app"),
                category_id: "polls".into(),
                title: "Which runtime?".into(),
                body: "vote".into(),
                poll_question: "Pick one".into(),
                poll_options: opts.into_iter().map(String::from).collect(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(make(vec!["only"]).await), "INVALID_ARGUMENT");
    assert_eq!(reason(make(vec!["a", "a"]).await), "INVALID_ARGUMENT");
    let disc = make(vec!["tokio", "smol", "glommio"])
        .await
        .unwrap()
        .into_inner();
    let poll = disc.poll.unwrap();
    assert_eq!(poll.options.len(), 3);
    let vote = |who: &str, opt: &str| {
        w.s.vote_discussion_poll(req(
            &w.tok(who),
            VoteDiscussionPollRequest {
                repo: rref("acme/app"),
                number: disc.number,
                option_id: opt.into(),
                ..Default::default()
            },
        ))
    };
    let tokio_id = poll.options[0].id.clone();
    let smol_id = poll.options[1].id.clone();
    vote("reader", &tokio_id).await.unwrap();
    vote("ci", &tokio_id).await.unwrap();
    let p = vote("reader", &tokio_id).await.unwrap().into_inner(); // same vote again: no change
    assert_eq!((p.total_votes, p.options[0].vote_count), (2, 2));
    // Changing the vote moves it.
    let p = vote("reader", &smol_id).await.unwrap().into_inner();
    assert_eq!(
        (
            p.total_votes,
            p.options[0].vote_count,
            p.options[1].vote_count
        ),
        (2, 1, 1)
    );
    assert!(p.viewer_has_voted && p.options[1].viewer_voted);
    assert_eq!(reason(vote("reader", "nope").await), "INVALID_ARGUMENT");
    // Others see their own flags.
    let mine =
        w.s.get_discussion_poll(req(
            &w.tok("ci"),
            GetDiscussionPollRequest {
                repo: rref("acme/app"),
                number: disc.number,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(mine.options[0].viewer_voted && !mine.options[1].viewer_voted);
    // Locked discussions take no votes.
    assert_eq!(
        reason(
            w.s.lock_discussion(req(
                &w.tok("reader"),
                LockDiscussionRequest {
                    repo: rref("acme/app"),
                    number: disc.number,
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    w.s.lock_discussion(req(
        &w.tok("triager"),
        LockDiscussionRequest {
            repo: rref("acme/app"),
            number: disc.number,
            reason: DiscussionLockReason::TooHeated as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(reason(vote("ci", &smol_id).await), "INVALID_STATE");
    w.s.unlock_discussion(req(
        &w.tok("triager"),
        UnlockDiscussionRequest {
            repo: rref("acme/app"),
            number: disc.number,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    vote("ci", &smol_id).await.unwrap();
    // Closed discussions take none either.
    w.s.close_discussion(req(
        &w.tok("writer"),
        CloseDiscussionRequest {
            repo: rref("acme/app"),
            number: disc.number,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(reason(vote("reader", &tokio_id).await), "INVALID_STATE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pins_locks_and_labels_are_moderator_tools() {
    let w = world().await;
    w.enable_discussions(&w.app).await;
    let d = D { w: &w };
    d.category("admin", "General", DiscussionCategoryFormat::OpenEnded)
        .await
        .unwrap();
    for i in 1..=5 {
        d.start("reader", "general", &format!("topic {i}"))
            .await
            .unwrap();
    }
    let pin = |who: &str, n: u64| {
        w.s.pin_discussion(req(
            &w.tok(who),
            PinDiscussionRequest {
                repo: rref("acme/app"),
                number: n,
                pattern: "dots".into(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(pin("writer", 1).await), "PERMISSION_DENIED");
    for n in 1..=4 {
        assert!(pin("admin", n).await.unwrap().into_inner().pinned);
    }
    assert_eq!(reason(pin("admin", 5).await), "TOO_LARGE");
    assert_eq!(reason(pin("admin", 1).await), "INVALID_STATE");
    let pinned =
        w.s.list_pinned_discussions(req(
            &w.tok("reader"),
            ListPinnedDiscussionsRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        pinned
            .discussions
            .iter()
            .map(|x| x.number)
            .collect::<Vec<_>>(),
        vec![4, 3, 2, 1]
    );
    w.s.unpin_discussion(req(
        &w.tok("admin"),
        UnpinDiscussionRequest {
            repo: rref("acme/app"),
            number: 4,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert!(pin("admin", 5).await.is_ok());
    // Locks stop non-moderators from commenting.
    w.s.lock_discussion(req(
        &w.tok("writer"),
        LockDiscussionRequest {
            repo: rref("acme/app"),
            number: 2,
            reason: DiscussionLockReason::OffTopic as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        d.comment("reader", 2, "late", "").await.unwrap_err(),
        "LOCKED"
    );
    assert!(d.comment("triager", 2, "moderating", "").await.is_ok());
    // Labels.
    let labels = |who: &str, l: &[&str]| {
        w.s.add_discussion_labels(req(
            &w.tok(who),
            AddDiscussionLabelsRequest {
                repo: rref("acme/app"),
                number: 3,
                labels: l.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        reason(labels("reader", &["bug"]).await),
        "PERMISSION_DENIED"
    );
    let l = labels("triager", &["bug", "help wanted", "bug"])
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        l.labels.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
        vec!["bug", "help wanted"]
    );
    let l =
        w.s.remove_discussion_labels(req(
            &w.tok("triager"),
            RemoveDiscussionLabelsRequest {
                repo: rref("acme/app"),
                number: 3,
                labels: vec!["BUG".into()],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(l.labels.len(), 1);
    let l =
        w.s.set_discussion_labels(req(
            &w.tok("triager"),
            SetDiscussionLabelsRequest {
                repo: rref("acme/app"),
                number: 3,
                labels: vec![],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(l.labels.is_empty());
    labels("triager", &["docs"]).await.unwrap();
    let by_label =
        w.s.list_discussions(req(
            &w.tok("reader"),
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                labels: vec!["docs".into()],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_label.discussions.len(), 1);
    // Paging.
    let p1 =
        w.s.list_discussions(req(
            &w.tok("reader"),
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                sort: DiscussionSort::Created as i32,
                page: Some(PageRequest {
                    page_size: 2,
                    page_token: String::new(),
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p1.discussions.len(), 2);
    let token = p1.page.unwrap().next_page_token;
    assert!(!token.is_empty());
    let p2 =
        w.s.list_discussions(req(
            &w.tok("reader"),
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                sort: DiscussionSort::Created as i32,
                page: Some(PageRequest {
                    page_size: 10,
                    page_token: token,
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p2.discussions.len(), 3);
    assert_eq!(p2.total_count, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_discussions_and_discussion_forms() {
    let w = world().await;
    // Org discussions: members participate, owners and admins moderate.
    let org_cat = |who: &str| {
        w.s.create_discussion_category(req(
            &w.tok(who),
            CreateDiscussionCategoryRequest {
                org: "acme".into(),
                name: "Town hall".into(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(org_cat("reader").await), "PERMISSION_DENIED");
    assert_eq!(reason(org_cat("outsider").await), "PERMISSION_DENIED");
    org_cat("admin").await.unwrap();
    let org_disc =
        w.s.create_discussion(req(
            &w.tok("reader"),
            CreateDiscussionRequest {
                org: "acme".into(),
                category_id: "town-hall".into(),
                title: "Offsite".into(),
                body: "where?".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            org_disc.number,
            org_disc.repo_id.is_empty(),
            org_disc.org_id.is_empty()
        ),
        (1, true, false)
    );
    // Org discussions have their own numbering and are not visible to non-members.
    assert_eq!(
        reason(
            w.s.get_discussion(req(
                &w.tok("outsider"),
                GetDiscussionRequest {
                    id: org_disc.id.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert!(
        w.s.get_discussion(req(
            &w.tok("reader"),
            GetDiscussionRequest {
                id: org_disc.id.clone(),
                ..Default::default()
            }
        ))
        .await
        .is_ok()
    );
    // A form on the default ref turns responses into the body and enforces required fields.
    w.enable_discussions(&w.app).await;
    D { w: &w }
        .category("admin", "Ideas", DiscussionCategoryFormat::OpenEnded)
        .await
        .unwrap();
    let form = "name: Ideas\nbody:\n  - type: markdown\n    attributes:\n      value: Tell us\n  - type: textarea\n    id: problem\n    attributes:\n      label: Problem\n    validations:\n      required: true\n  - type: textarea\n    id: sketch\n    attributes:\n      label: Sketch\n";
    w.vcs
        .seed(
            &w.vr(&w.app),
            &[(".github/DISCUSSION_TEMPLATE/ideas.yml", form)],
            &human("admin"),
            "add form",
            T0,
        )
        .await
        .unwrap();
    let with = |responses: Vec<(&str, &str)>| {
        w.s.create_discussion(req(
            &w.tok("reader"),
            CreateDiscussionRequest {
                repo: rref("acme/app"),
                category_id: "ideas".into(),
                title: "faster land".into(),
                form_responses: responses
                    .into_iter()
                    .map(|(k, v)| KeyValue {
                        key: k.into(),
                        value: v.into(),
                    })
                    .collect(),
                ..Default::default()
            },
        ))
    };
    let err = with(vec![("sketch", "boxes")]).await;
    let detail = bee_proto::errors::detail_from_status(&err.unwrap_err()).unwrap();
    assert_eq!(
        (detail.reason.as_str(), detail.subcode.as_str()),
        ("INVALID_ARGUMENT", "form_field_required")
    );
    let made = with(vec![("problem", "land is slow"), ("sketch", "a diagram")])
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        made.body,
        "### Problem\n\nland is slow\n\n### Sketch\n\na diagram"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issues_convert_to_discussions_and_back_through_the_bridge() {
    let w = world().await;
    w.enable_discussions(&w.app).await;
    let d = D { w: &w };
    d.category("admin", "General", DiscussionCategoryFormat::OpenEnded)
        .await
        .unwrap();
    d.category("admin", "Polls", DiscussionCategoryFormat::Poll)
        .await
        .unwrap();
    w.issues.add(
        &w.app.id,
        IssueSnapshot {
            number: 7,
            title: "Should we do X?".into(),
            body: "ping @reader-login".into(),
            author: human("reader"),
            labels: vec!["question".into()],
        },
    );
    let convert = |who: &str, cat: &str| {
        w.s.convert_issue_to_discussion(req(
            &w.tok(who),
            ConvertIssueToDiscussionRequest {
                repo: rref("acme/app"),
                issue_number: 7,
                category_id: cat.into(),
            },
        ))
    };
    assert_eq!(
        reason(convert("reader", "general").await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(convert("triager", "polls").await),
        "INVALID_ARGUMENT"
    );
    let disc = convert("triager", "general").await.unwrap().into_inner();
    assert_eq!(disc.title, "Should we do X?");
    assert_eq!(disc.author.unwrap().id, "reader");
    assert_eq!(disc.converted_from.unwrap().id, "7");
    assert_eq!(disc.labels[0].name, "question");
    assert_eq!(
        reason(convert("triager", "general").await),
        "ALREADY_EXISTS"
    );
    // And back: the issue is opened through the bridge and linked from the discussion.
    let back =
        w.s.convert_discussion_to_issue(req(
            &w.tok("triager"),
            ConvertDiscussionToIssueRequest {
                repo: rref("acme/app"),
                number: disc.number,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let issue_ref = back.issue.unwrap();
    assert_eq!(issue_ref.kind, SubjectKind::Issue as i32);
    let created = w
        .issues
        .get(&w.app.id, issue_ref.id.parse().unwrap())
        .unwrap();
    assert!(created.body.contains("Converted from discussion"));
    assert_eq!(back.discussion.unwrap().linked_issues.len(), 1);
    // Work consumes events to close and link the originals.
    w.flush().await;
    let kinds = w.event_kinds().await;
    assert!(kinds.contains(&"discussion.converted_from_issue".to_string()));
    assert!(kinds.contains(&"discussion.converted_to_issue".to_string()));
}
