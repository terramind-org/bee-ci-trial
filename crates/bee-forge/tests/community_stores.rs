//! The community stores on every backend: memory and SQLite always, Postgres when
//! `BEE_TEST_PG` holds a connection URL. One scenario touches the tables of each area
//! (unique keys, counters, row locks, cursors, outboxes) through the services.

use crate::community_support as support;

use std::sync::Arc;

use bee_forge::ids::SystemClock;
use bee_forge::module::Registry;
use bee_forge::store::ForgeStore;
use bee_forge::store::memory::MemoryStore;
use bee_forge::store::sql::{SqlConfig, SqlStore};
use bee_proto::v1::discussion_service_server::DiscussionService;
use bee_proto::v1::gist_service_server::GistService;
use bee_proto::v1::notification_service_server::NotificationService;
use bee_proto::v1::release_service_server::ReleaseService;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::wiki_service_server::WikiService;
use bee_proto::v1::*;
use support::*;

async fn scenario(store: Arc<dyn ForgeStore>) {
    let w = world_on(store).await;
    let rd = &w.tok("reader");
    let wr = &w.tok("writer");
    let ad = &w.tok("admin");
    w.enable_discussions(&w.app).await;
    // Notifications need a watcher first.
    w.s.set_repo_watch(req(
        rd,
        SetRepoWatchRequest {
            repo: rref("acme/app"),
            level: RepoWatchLevel::All as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    // Discussions: unique slugs, per-home numbers, counters, comments, answers, polls.
    w.s.create_discussion_category(req(
        ad,
        CreateDiscussionCategoryRequest {
            repo: rref("acme/app"),
            name: "Q&A".into(),
            format: DiscussionCategoryFormat::QuestionAnswer as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.create_discussion_category(req(
                ad,
                CreateDiscussionCategoryRequest {
                    repo: rref("acme/app"),
                    name: "q&a".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    let mut numbers = Vec::new();
    for i in 0..3 {
        let d =
            w.s.create_discussion(req(
                wr,
                CreateDiscussionRequest {
                    repo: rref("acme/app"),
                    category_id: "q-a".into(),
                    title: format!("q{i}"),
                    body: "b".into(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner();
        numbers.push(d.number);
    }
    assert_eq!(numbers, vec![1, 2, 3]);
    let c =
        w.s.create_discussion_comment(req(
            rd,
            CreateDiscussionCommentRequest {
                repo: rref("acme/app"),
                number: 1,
                body: "answer".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let marked =
        w.s.mark_discussion_answer(req(
            wr,
            MarkDiscussionAnswerRequest {
                comment_id: c.comment.unwrap().id,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((marked.comment_count, marked.version >= 3), (1, true));
    w.s.add_discussion_upvote(req(
        rd,
        AddDiscussionUpvoteRequest {
            repo: rref("acme/app"),
            number: 2,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let top =
        w.s.list_discussions(req(
            rd,
            ListDiscussionsRequest {
                repo: rref("acme/app"),
                sort: DiscussionSort::Top as i32,
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
    assert_eq!(
        (
            top.discussions[0].number,
            top.total_count,
            top.page.unwrap().next_page_token.is_empty()
        ),
        (2, 3, false)
    );
    // Gists: two scopes, forks and counters.
    let g =
        w.s.create_gist(req(
            rd,
            CreateGistRequest {
                files: [("a.rs".to_string(), "fn a() {}".to_string())].into(),
                public: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    w.s.fork_gist(req(
        wr,
        ForkGistRequest {
            gist_id: g.id.clone(),
        },
    ))
    .await
    .unwrap();
    w.s.star_gist(req(
        wr,
        StarGistRequest {
            gist_id: g.id.clone(),
        },
    ))
    .await
    .unwrap();
    let got =
        w.s.get_gist(req(
            rd,
            GetGistRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((got.fork_count, got.star_count), (1, 1));
    // Wiki: pages through the VCS port, switch in the store.
    w.s.enable_wiki(req(
        ad,
        EnableWikiRequest {
            repo: rref("acme/app"),
            edit_access: 0,
        },
    ))
    .await
    .unwrap();
    w.s.create_wiki_page(req(
        wr,
        CreateWikiPageRequest {
            repo: rref("acme/app"),
            title: "Home".into(),
            content: "hi".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    // Releases: unique tags, latest ranking, asset keys.
    w.vcs
        .seed(&w.vr(&w.app), &[("a", "1\n")], &human("writer"), "init", T0)
        .await
        .unwrap();
    let rel =
        w.s.create_release(req(
            wr,
            CreateReleaseRequest {
                repo: rref("acme/app"),
                tag_name: "v1".into(),
                target: Some(Revision {
                    expr: "main".into(),
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(rel.latest);
    assert_eq!(
        reason(
            w.s.create_release(req(
                wr,
                CreateReleaseRequest {
                    repo: rref("acme/app"),
                    tag_name: "v1".into(),
                    target: Some(Revision {
                        expr: "main".into()
                    }),
                    ..Default::default()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    let asset =
        w.s.begin_release_asset_upload(req(
            wr,
            BeginReleaseAssetUploadRequest {
                repo: rref("acme/app"),
                release_id: rel.id.clone(),
                name: "a.zip".into(),
                size: 5,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        reason(
            w.s.begin_release_asset_upload(req(
                wr,
                BeginReleaseAssetUploadRequest {
                    repo: rref("acme/app"),
                    release_id: rel.id.clone(),
                    name: "a.zip".into(),
                    size: 5,
                    ..Default::default()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    w.s.finalize_release_asset(req(
        wr,
        FinalizeReleaseAssetRequest {
            repo: rref("acme/app"),
            asset_id: asset.asset_id,
            sha256: vec![3; 32],
        },
    ))
    .await
    .unwrap();
    // Users: unique logins and emails, counters, redirects.
    w.s.follow_user(req(
        rd,
        FollowUserRequest {
            login: "writer-login".into(),
        },
    ))
    .await
    .unwrap();
    w.s.star_repo(req(
        rd,
        StarRepoRequest {
            repo: rref("acme/open"),
        },
    ))
    .await
    .unwrap();
    w.s.rename_user_login(req(
        rd,
        RenameUserLoginRequest {
            new_login: "rita".into(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        w.s.get_user(req(
            wr,
            GetUserRequest {
                user: Some(get_user_request::User::Login("reader-login".into()))
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .login,
        "rita"
    );
    assert_eq!(
        reason(
            w.s.rename_user_login(req(
                wr,
                RenameUserLoginRequest {
                    new_login: "RITA".into()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    // Events flow through the relay into the logs, the fan-out reads them by cursor.
    w.flush().await;
    w.flush().await;
    let threads =
        w.s.list_notification_threads(req(rd, ListNotificationThreadsRequest::default()))
            .await
            .unwrap()
            .into_inner()
            .threads;
    assert!(
        threads
            .iter()
            .any(|t| t.subject.as_ref().unwrap().kind == SubjectKind::Discussion as i32)
    );
    assert!(
        threads
            .iter()
            .any(|t| t.subject.as_ref().unwrap().kind == SubjectKind::Release as i32)
    );
    let count =
        w.s.get_notification_unread_count(req(rd, GetNotificationUnreadCountRequest::default()))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(
        count.total as usize,
        threads.iter().filter(|t| t.unread).count()
    );
    w.s.mark_notification_threads_done(req(
        rd,
        MarkNotificationThreadsDoneRequest {
            selector: Some(NotificationThreadSelector {
                thread_ids: threads.iter().map(|t| t.id.clone()).collect(),
                ..Default::default()
            }),
        },
    ))
    .await
    .unwrap();
    assert!(
        w.s.list_notification_threads(req(rd, ListNotificationThreadsRequest::default()))
            .await
            .unwrap()
            .into_inner()
            .threads
            .is_empty()
    );
    // The community module is registered with its three surfaces and migrations.
    let reg = Registry::core().unwrap();
    for s in ["community", "notifications", "users"] {
        assert!(reg.surfaces().iter().any(|x| x.name() == s), "{s}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_community_scenario() {
    scenario(Arc::new(MemoryStore::new())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_community_scenario() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqlStore::open(SqlConfig::sqlite(dir.path()), Arc::new(SystemClock))
        .await
        .unwrap();
    scenario(Arc::new(store)).await;
    // One database file per community surface, separate from the others.
    for s in ["community", "notifications", "users"] {
        assert!(dir.path().join(format!("forge-{s}.db")).exists(), "{s}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_community_scenario_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };
    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let bee_forge::store::sql::SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_community_{}", std::process::id());
    }
    cfg.default_pool_size = 4;
    scenario(Arc::new(
        SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap(),
    ))
    .await;
}
