//! Coverage for PRODUCT §4.16 rows that had no test: activity derived from the event logs
//! (contribution graph and overview, home feed, achievements) and repo metadata through the
//! forge services (description, delete and restore within 90 days, default branch).

use crate::community_support as support;

use bee_forge::Cx;
use bee_forge::ids::ts;
use bee_proto::v1::event::Body;
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::*;
use support::*;

const DAY: u64 = 86_400_000;
const HOUR: u64 = 3_600_000;

/// Appends one event to the forge's logs at `at_ms`.
async fn event(w: &World, kind: &str, who: &str, repo: &Repo, body: Option<Body>, at_ms: u64) {
    event_json(w, kind, who, repo, body, "", at_ms).await;
}

async fn event_json(
    w: &World,
    kind: &str,
    who: &str,
    repo: &Repo,
    body: Option<Body>,
    payload: &str,
    at_ms: u64,
) {
    let dedupe = format!("{kind}/{who}/{}/{at_ms}", repo.id);
    w.env
        .forge
        .events
        .append(
            &Cx::system(),
            Event {
                kind: kind.into(),
                principal: who.into(),
                repo_id: repo.id.clone(),
                org_id: repo.org_id.clone(),
                time: Some(ts(at_ms)),
                body,
                subject: Some(SubjectRef {
                    kind: SubjectKind::Repo as i32,
                    repo: rid(&repo.id),
                    id: format!("{kind}@{at_ms}"),
                }),
                payload_json: payload.into(),
                ..Default::default()
            },
            &dedupe,
        )
        .await
        .unwrap();
}

fn push(name: &str) -> Option<Body> {
    Some(Body::RefUpdated(Ref {
        name: name.into(),
        target: Some(ObjectId { raw: vec![7; 32] }),
        version: 2,
    }))
}

fn cr(number: u64, author: &str) -> Option<Body> {
    Some(Body::Change(ChangeRequest {
        number,
        title: format!("change {number}"),
        author: Some(Principal {
            id: author.into(),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

fn review(change: &str) -> Option<Body> {
    Some(Body::Review(Review {
        change_id: change.into(),
        ..Default::default()
    }))
}

async fn graph(
    w: &World,
    who: &str,
    req_: GetUserContributionGraphRequest,
) -> Result<UserContributionGraph, String> {
    w.s.get_user_contribution_graph(req(&w.tok(who), req_))
        .await
        .map(|r| r.into_inner())
        .map_err(|e| {
            bee_proto::errors::detail_from_status(&e)
                .map(|d| d.reason)
                .unwrap_or_default()
        })
}

fn day<'a>(g: &'a UserContributionGraph, date: &str) -> &'a UserContributionDay {
    g.days
        .iter()
        .find(|d| d.date == date)
        .expect("day in range")
}

/// The writer's activity: a push to the private repo two days ago, a change request, a review
/// and an issue in the public repo; tags and other people's work do not count.
async fn seed_writer(w: &World) {
    let two_days = T0 - 2 * DAY - 11 * HOUR; // 2026-10-16 01:00 UTC
    event(w, "ref.updated", "writer", &w.app, push("main"), two_days).await;
    event(
        w,
        "ref.updated",
        "writer",
        &w.app,
        push("tags/v1"),
        two_days,
    )
    .await;
    event(
        w,
        "cr.opened",
        "writer",
        &w.open,
        cr(1, "writer"),
        two_days + HOUR,
    )
    .await;
    event(
        w,
        "review.submitted",
        "writer",
        &w.open,
        review("cr-9"),
        two_days + 2 * HOUR,
    )
    .await;
    event_json(
        w,
        "issue.opened",
        "writer",
        &w.open,
        None,
        r#"{"number":"4","title":"crash"}"#,
        T0,
    )
    .await;
    event(w, "cr.opened", "admin", &w.open, cr(2, "admin"), T0).await;
    w.flush().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contribution_graph_counts_landed_updates_changes_reviews_and_issues_by_day() {
    let w = world().await;
    seed_writer(&w).await;

    // The owner reads everything: a year of days ending today.
    let g = graph(
        &w,
        "owner",
        GetUserContributionGraphRequest {
            login: "writer-login".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(g.login, "writer-login");
    assert_eq!((g.to_date.as_str(), g.days.len()), ("2026-10-18", 365));
    assert_eq!(g.total, 4);
    assert_eq!(
        (day(&g, "2026-10-16").count, day(&g, "2026-10-16").level),
        (3, 4)
    );
    assert_eq!(
        (day(&g, "2026-10-18").count, day(&g, "2026-10-18").level),
        (1, 2)
    );
    assert_eq!(day(&g, "2026-10-17").count, 0);
    assert!(!g.includes_private);

    // Days follow the requested time zone: 01:00-03:00 UTC is the previous evening in Los
    // Angeles.
    let la = graph(
        &w,
        "owner",
        GetUserContributionGraphRequest {
            login: "writer-login".into(),
            time_zone: "America/Los_Angeles".into(),
            from_date: "2026-10-15".into(),
            to_date: "2026-10-18".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let counts: Vec<(String, u32)> = la.days.iter().map(|d| (d.date.clone(), d.count)).collect();
    assert_eq!(
        counts,
        vec![
            ("2026-10-15".to_string(), 3),
            ("2026-10-16".to_string(), 0),
            ("2026-10-17".to_string(), 0),
            ("2026-10-18".to_string(), 1),
        ]
    );

    // The outsider cannot read `acme/app`: the push is left out ...
    let g = graph(
        &w,
        "outsider",
        GetUserContributionGraphRequest {
            login: "writer-login".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!((g.total, g.includes_private), (3, false));
    // ... until the writer chooses to show private contributions (counted anonymously).
    w.s.update_user_settings(req(
        &w.tok("writer"),
        UpdateUserSettingsRequest {
            show_private_contributions: Some(true),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let g = graph(
        &w,
        "outsider",
        GetUserContributionGraphRequest {
            login: "writer-login".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!((g.total, g.includes_private), (4, true));

    // Org filter, bad input.
    let other = graph(
        &w,
        "owner",
        GetUserContributionGraphRequest {
            login: "writer-login".into(),
            org: "acme".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(other.total, 4);
    for bad in [
        GetUserContributionGraphRequest {
            from_date: "2026-10-19".into(),
            to_date: "2026-10-18".into(),
            ..Default::default()
        },
        GetUserContributionGraphRequest {
            from_date: "2024-01-01".into(),
            to_date: "2026-10-18".into(),
            ..Default::default()
        },
        GetUserContributionGraphRequest {
            to_date: "18/10/2026".into(),
            ..Default::default()
        },
    ] {
        assert_eq!(
            graph(&w, "owner", bad).await.unwrap_err(),
            "INVALID_ARGUMENT"
        );
    }
    assert_eq!(
        graph(
            &w,
            "owner",
            GetUserContributionGraphRequest {
                login: "nobody".into(),
                ..Default::default()
            }
        )
        .await
        .unwrap_err(),
        "NOT_FOUND"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activity_overview_shares_and_top_repos_respect_read_access() {
    let w = world().await;
    seed_writer(&w).await;
    let ov = |who: &'static str| {
        let w = &w;
        async move {
            w.s.get_user_activity_overview(req(
                &w.tok(who),
                GetUserActivityOverviewRequest {
                    login: "writer-login".into(),
                    ..Default::default()
                },
            ))
            .await
            .unwrap()
            .into_inner()
        }
    };
    let o = ov("owner").await;
    assert_eq!(
        (
            o.changesets_percent,
            o.change_requests_percent,
            o.reviews_percent,
            o.issues_percent
        ),
        (25.0, 25.0, 25.0, 25.0)
    );
    assert_eq!(o.top_repos, vec!["acme/open", "acme/app"]);
    assert_eq!(o.top_orgs, vec!["acme"]);
    assert_eq!(o.to_date, "2026-10-18");
    let o = ov("outsider").await;
    assert_eq!(o.changesets_percent, 0.0);
    assert!((o.reviews_percent - 100.0 / 3.0).abs() < 1e-9);
    assert_eq!(o.top_repos, vec!["acme/open"]);
}

async fn feed(
    w: &World,
    who: &str,
    kinds: &[&str],
    size: u32,
    token: &str,
) -> (Vec<UserFeedEvent>, String) {
    let r =
        w.s.list_user_feed_events(req(
            &w.tok(who),
            ListUserFeedEventsRequest {
                kinds: kinds.iter().map(|k| k.to_string()).collect(),
                page: Some(PageRequest {
                    page_size: size,
                    page_token: token.into(),
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    (r.events, r.page.unwrap_or_default().next_page_token)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn home_feed_merges_followed_users_and_starred_repos_newest_first() {
    let w = world().await;
    let me = w.tok("outsider");
    // Nothing followed or starred: an empty feed.
    assert!(feed(&w, "outsider", &[], 10, "").await.0.is_empty());
    w.s.follow_user(req(
        &me,
        FollowUserRequest {
            login: "writer-login".into(),
        },
    ))
    .await
    .unwrap();
    w.s.star_repo(req(
        &me,
        StarRepoRequest {
            repo: rref("acme/open"),
        },
    ))
    .await
    .unwrap();
    // The writer acts in both repos and follows someone; the admin lands the writer's change
    // in the starred repo and pushes to the private one.
    event(
        &w,
        "cr.opened",
        "writer",
        &w.open,
        cr(1, "writer"),
        T0 + HOUR,
    )
    .await;
    event(
        &w,
        "ref.updated",
        "writer",
        &w.app,
        push("main"),
        T0 + 2 * HOUR,
    )
    .await;
    event(
        &w,
        "cr.landed",
        "admin",
        &w.open,
        cr(1, "writer"),
        T0 + 3 * HOUR,
    )
    .await;
    event(
        &w,
        "ref.updated",
        "admin",
        &w.app,
        push("main"),
        T0 + 4 * HOUR,
    )
    .await;
    w.clock.advance(5 * HOUR);
    w.s.follow_user(req(
        &w.tok("writer"),
        FollowUserRequest {
            login: "reader-login".into(),
        },
    ))
    .await
    .unwrap();
    w.flush().await;

    let (all, next) = feed(&w, "outsider", &[], 50, "").await;
    assert!(next.is_empty());
    let got: Vec<(String, String)> = all
        .iter()
        .map(|e| (e.kind.clone(), e.actor.clone().unwrap_or_default().id))
        .collect();
    // Newest first; the private push is hidden; the outsider's own follow and star are not
    // in its feed; the landing shows once.
    assert_eq!(
        got,
        vec![
            ("follow.added".to_string(), "writer".to_string()),
            ("cr.landed".to_string(), "admin".to_string()),
            ("cr.opened".to_string(), "writer".to_string()),
        ]
    );
    assert_eq!(all[2].summary, "opened change request #1: change 1");
    assert_eq!(all[1].created_at, Some(ts(T0 + 3 * HOUR)));
    assert_eq!(all[0].actor.clone().unwrap().display, "writer");

    // Paging one at a time walks the same entries without repeats.
    let mut walked = Vec::new();
    let mut token = String::new();
    loop {
        let (page, next) = feed(&w, "outsider", &[], 1, &token).await;
        walked.extend(page.into_iter().map(|e| e.id));
        if next.is_empty() {
            break;
        }
        token = next;
    }
    assert_eq!(walked, all.iter().map(|e| e.id.clone()).collect::<Vec<_>>());

    // Kind filters.
    let (crs, _) = feed(&w, "outsider", &["cr.*"], 50, "").await;
    assert_eq!(crs.len(), 2);

    // A reader of the private repo sees the writer's push there too.
    w.s.follow_user(req(
        &w.tok("owner"),
        FollowUserRequest {
            login: "writer-login".into(),
        },
    ))
    .await
    .unwrap();
    let (own, _) = feed(&w, "owner", &["ref.*"], 50, "").await;
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].actor.clone().unwrap().id, "writer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn achievements_are_derived_from_events_with_tiers() {
    let w = world().await;
    for i in 0..10u64 {
        event(
            &w,
            "review.submitted",
            "writer",
            &w.open,
            review(&format!("cr-{i}")),
            T0 + i * HOUR,
        )
        .await;
    }
    event(
        &w,
        "cr.landed",
        "admin",
        &w.open,
        cr(5, "writer"),
        T0 + 20 * HOUR,
    )
    .await;
    event(
        &w,
        "ref.updated",
        "writer",
        &w.app,
        push("main"),
        T0 + 21 * HOUR,
    )
    .await;
    w.clock.advance(DAY);
    w.flush().await;
    let list = |who: &'static str| {
        let w = &w;
        async move {
            w.s.list_user_achievements(req(
                &w.tok(who),
                ListUserAchievementsRequest {
                    login: "writer-login".into(),
                },
            ))
            .await
            .unwrap()
            .into_inner()
            .achievements
        }
    };
    let mine = list("owner").await;
    let got: Vec<(String, String, u32)> = mine
        .iter()
        .map(|a| (a.slug.clone(), a.tier.clone(), a.count))
        .collect();
    assert_eq!(
        got,
        vec![
            ("first-landing".to_string(), "base".to_string(), 1),
            ("shipped".to_string(), "base".to_string(), 1),
            ("second-pair-of-eyes".to_string(), "bronze".to_string(), 10),
        ]
    );
    let reviews = &mine[2];
    assert_eq!(
        reviews.unlocked_at,
        Some(ts(T0)),
        "the first review unlocked it"
    );
    assert_eq!(
        reviews.unlocked_by.as_ref().unwrap().id,
        format!("review.submitted@{T0}")
    );
    assert!(
        reviews
            .image_url
            .ends_with("second-pair-of-eyes-bronze.png")
    );
    // The push to the private repo earns nothing the outsider can see.
    let theirs: Vec<String> = list("outsider").await.into_iter().map(|a| a.slug).collect();
    assert_eq!(theirs, vec!["shipped", "second-pair-of-eyes"]);
    // The admin, who only landed, has no achievement for it.
    let admin =
        w.s.list_user_achievements(req(
            &w.tok("owner"),
            ListUserAchievementsRequest {
                login: "admin-login".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(admin.achievements.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn descriptions_are_set_at_creation_edited_and_listed() {
    let w = world().await;
    let owner = w.tok("owner");
    let made =
        w.s.create_repo(req(
            &owner,
            CreateRepoRequest {
                org: "acme".into(),
                name: "described".into(),
                description: "first words".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(made.description, "first words");
    let edited =
        w.s.update_repo(req(
            &owner,
            UpdateRepoRequest {
                repo: rref("acme/described"),
                description: "better words".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(edited.description, "better words");
    let got =
        w.s.get_repo(req(
            &w.tok("admin"),
            GetRepoRequest {
                repo: rref("acme/described"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.description, "better words");
    let listed =
        w.s.list_repos(req(
            &owner,
            ListReposRequest {
                org: "acme".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let d: Vec<(String, String)> = listed
        .repos
        .iter()
        .map(|r| (r.name.clone(), r.description.clone()))
        .collect();
    assert!(d.contains(&("described".to_string(), "better words".to_string())));
    assert!(d.contains(&("open".to_string(), "the open repo".to_string())));
    // Writers cannot change it.
    assert_eq!(
        reason(
            w.s.update_repo(req(
                &w.tok("writer"),
                UpdateRepoRequest {
                    repo: rref("acme/open"),
                    description: "vandalized".into(),
                    ..Default::default()
                },
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_branch_is_the_repos_default_ref() {
    let w = world().await;
    let owner = w.tok("owner");
    assert_eq!(w.open.default_ref, "main");
    let moved =
        w.s.update_repo(req(
            &owner,
            UpdateRepoRequest {
                repo: rref("acme/open"),
                default_ref: Some("trunk".into()),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(moved.default_ref, "trunk");
    let got =
        w.s.get_repo(req(
            &w.tok("reader"),
            GetRepoRequest {
                repo: rref("acme/open"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.default_ref, "trunk");
    assert_eq!(
        reason(
            w.s.update_repo(req(
                &w.tok("writer"),
                UpdateRepoRequest {
                    repo: rref("acme/open"),
                    default_ref: Some("mine".into()),
                    ..Default::default()
                },
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_profile_fields_readme_and_pinned_repos() {
    let w = world().await;
    let owner = w.tok("owner");
    let org =
        w.s.get_org(req(
            &owner,
            GetOrgRequest {
                name_or_id: "acme".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let org =
        w.s.update_org(req(
            &owner,
            UpdateOrgRequest {
                org: "acme".into(),
                description: Some("We build things".into()),
                website: Some("https://acme.example".into()),
                location: Some("Pune".into()),
                expected_version: org.version,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            org.description.as_str(),
            org.website.as_str(),
            org.location.as_str()
        ),
        ("We build things", "https://acme.example", "Pune")
    );
    let st =
        w.s.get_org_settings(req(&owner, GetOrgSettingsRequest { org: "acme".into() }))
            .await
            .unwrap()
            .into_inner();
    let st =
        w.s.update_org_settings(req(
            &owner,
            UpdateOrgSettingsRequest {
                org: "acme".into(),
                readme_repo_id: Some(w.open.id.clone()),
                set_pinned_repo_ids: true,
                pinned_repo_ids: vec![w.open.id.clone(), w.app.id.clone()],
                expected_version: st.version,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(st.readme_repo_id, w.open.id);
    assert_eq!(
        st.pinned_repo_ids,
        vec![w.open.id.clone(), w.app.id.clone()]
    );
    let seen =
        w.s.get_org_settings(req(
            &w.tok("reader"),
            GetOrgSettingsRequest { org: "acme".into() },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(seen.pinned_repo_ids.len(), 2);
}
