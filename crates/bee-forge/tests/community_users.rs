//! `UserService`, `SponsorService` and `InsightsService`: profiles and the social graph,
//! emails and commit attribution, login renames, exports and deletion, sponsorships with
//! sponsor-only repo access, FUNDING.yml, and repo insights.

use crate::community_support as support;

use bee_forge::Cx;
use bee_proto::v1::gist_service_server::GistService;
use bee_proto::v1::user_service_server::UserService;
use bee_proto::v1::*;
use support::*;

const DAY: u64 = 86_400_000;

async fn viewer(w: &World, who: &str) -> UserProfile {
    w.s.get_viewer(req(&w.tok(who), GetViewerRequest {}))
        .await
        .unwrap()
        .into_inner()
}

async fn user(w: &World, who: &str, login: &str) -> Result<UserProfile, String> {
    w.s.get_user(req(
        &w.tok(who),
        GetUserRequest {
            user: Some(get_user_request::User::Login(login.into())),
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|st| reason::<()>(Err(st)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn profiles_status_pins_and_avatars() {
    let w = world().await;
    let v = viewer(&w, "reader").await;
    assert_eq!(
        (v.login.as_str(), v.principal.as_ref().unwrap().id.as_str()),
        ("reader-login", "reader")
    );
    assert_eq!(v.orgs, vec!["acme".to_string()]); // org badge
    assert_eq!(v.follower_count + v.following_count + v.star_count, 0);
    // Profile fields validate.
    let upd = |f: &dyn Fn(&mut UpdateUserProfileRequest)| {
        let mut r = UpdateUserProfileRequest::default();
        f(&mut r);
        w.s.update_user_profile(req(&w.tok("reader"), r))
    };
    assert_eq!(
        reason(upd(&|r| r.website = Some("javascript:alert(1)".into())).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(upd(&|r| r.bio = Some("x".repeat(501))).await),
        "TOO_LARGE"
    );
    assert_eq!(
        reason(upd(&|r| r.public_email = Some("reader@example.com".into())).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(
            upd(&|r| {
                r.replace_social_accounts = true;
                r.social_accounts = (0..5)
                    .map(|i| UserSocialAccount {
                        provider: "x".into(),
                        url: format!("https://x.example/{i}"),
                    })
                    .collect();
            })
            .await
        ),
        "TOO_LARGE"
    );
    let p = upd(&|r| {
        r.name = Some("Rita Reader".into());
        r.bio = Some("I review things".into());
        r.pronouns = Some("she/her".into());
        r.company = Some("Acme".into());
        r.website = Some("https://rita.example".into());
        r.hireable = Some(true);
        r.replace_social_accounts = true;
        r.social_accounts = vec![UserSocialAccount {
            provider: "mastodon".into(),
            url: "https://m.example/@rita".into(),
        }];
    })
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        (
            p.name.as_str(),
            p.pronouns.as_str(),
            p.hireable,
            p.social_accounts.len()
        ),
        ("Rita Reader", "she/her", true, 1)
    );
    // Others see it by login; unknown logins are NOT_FOUND.
    let seen = user(&w, "outsider", "reader-login").await.unwrap();
    assert_eq!(seen.bio, "I review things");
    assert_eq!(
        user(&w, "outsider", "nobody").await.unwrap_err(),
        "NOT_FOUND"
    );
    // Avatar upload: content type and size are checked; finalizing sets the URL.
    let begin = |ct: &str, size: u64| {
        w.s.begin_user_avatar_upload(req(
            &w.tok("reader"),
            BeginUserAvatarUploadRequest {
                content_type: ct.into(),
                size,
            },
        ))
    };
    assert_eq!(reason(begin("text/html", 10).await), "INVALID_ARGUMENT");
    assert_eq!(reason(begin("image/png", 5 << 20).await), "TOO_LARGE");
    let up = begin("image/png", 2048).await.unwrap().into_inner();
    assert!(up.upload_url.contains("signature="));
    assert_eq!(
        reason(
            w.s.finalize_user_avatar_upload(req(
                &w.tok("reader"),
                FinalizeUserAvatarUploadRequest {
                    upload_id: up.upload_id.clone(),
                    sha256: vec![1; 5]
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(
            w.s.finalize_user_avatar_upload(req(
                &w.tok("writer"),
                FinalizeUserAvatarUploadRequest {
                    upload_id: up.upload_id.clone(),
                    sha256: vec![1; 32]
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let with_avatar =
        w.s.finalize_user_avatar_upload(req(
            &w.tok("reader"),
            FinalizeUserAvatarUploadRequest {
                upload_id: up.upload_id,
                sha256: vec![1; 32],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        with_avatar
            .avatar_url
            .starts_with("https://blobs.beehive.invalid/avatars/reader/")
    );
    // Status: set, expire, clear.
    let expiry = Some(bee_forge::ids::ts(T0 + DAY));
    let st =
        w.s.set_user_status(req(
            &w.tok("reader"),
            SetUserStatusRequest {
                message: "on vacation".into(),
                emoji: ":palm_tree:".into(),
                busy: true,
                expires_at: expiry,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(st.busy && st.updated_at.is_some());
    assert_eq!(
        user(&w, "outsider", "reader-login")
            .await
            .unwrap()
            .status
            .unwrap()
            .message,
        "on vacation"
    );
    w.clock.advance(2 * DAY);
    assert!(viewer(&w, "reader").await.status.is_none());
    assert_eq!(
        reason(
            w.s.set_user_status(req(
                &w.tok("reader"),
                SetUserStatusRequest {
                    message: "x".into(),
                    expires_at: Some(bee_forge::ids::ts(T0)),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    w.s.set_user_status(req(
        &w.tok("reader"),
        SetUserStatusRequest {
            message: "back".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    w.s.clear_user_status(req(&w.tok("reader"), ClearUserStatusRequest {}))
        .await
        .unwrap();
    assert!(viewer(&w, "reader").await.status.is_none());
    // Pinned items: repos the caller can read and gists they own.
    let gist =
        w.s.create_gist(req(
            &w.tok("reader"),
            CreateGistRequest {
                files: [("a.txt".to_string(), "x".to_string())].into(),
                public: true,
                description: "my gist".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let others =
        w.s.create_gist(req(
            &w.tok("writer"),
            CreateGistRequest {
                files: [("b.txt".to_string(), "y".to_string())].into(),
                public: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let pin = |who: &str, items: Vec<(UserPinnedItemKind, &str)>| {
        w.s.set_user_pinned_items(req(
            &w.tok(who),
            SetUserPinnedItemsRequest {
                items: items
                    .into_iter()
                    .map(|(k, id)| UserPinnedItemRef {
                        kind: k as i32,
                        id: id.into(),
                    })
                    .collect(),
            },
        ))
    };
    assert_eq!(
        reason(pin("reader", vec![(UserPinnedItemKind::Gist, &others.id)]).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(pin("outsider", vec![(UserPinnedItemKind::Repo, "acme/app")]).await),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(
            pin(
                "reader",
                vec![
                    (UserPinnedItemKind::Repo, "acme/app"),
                    (UserPinnedItemKind::Repo, "acme/app")
                ]
            )
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let pins = pin(
        "reader",
        vec![
            (UserPinnedItemKind::Repo, "acme/app"),
            (UserPinnedItemKind::Repo, "acme/open"),
            (UserPinnedItemKind::Gist, &gist.id),
        ],
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        pins.items
            .iter()
            .map(|i| i.title.as_str())
            .collect::<Vec<_>>(),
        vec!["acme/app", "acme/open", "my gist"]
    );
    // Viewers who cannot read a pinned private repo do not see it.
    let public_view =
        w.s.list_user_pinned_items(req(
            &w.tok("outsider"),
            ListUserPinnedItemsRequest {
                login: "reader-login".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        public_view
            .items
            .iter()
            .map(|i| i.title.as_str())
            .collect::<Vec<_>>(),
        vec!["acme/open", "my gist"]
    );
    assert_eq!(
        reason(
            pin(
                "reader",
                (0..7)
                    .map(|i| (
                        UserPinnedItemKind::Gist,
                        if i == 0 { gist.id.as_str() } else { "x" }
                    ))
                    .collect()
            )
            .await
        ),
        "TOO_LARGE"
    );
    assert_eq!(
        user(&w, "outsider", "reader-login")
            .await
            .unwrap()
            .public_gist_count,
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follows_stars_and_star_lists() {
    let w = world().await;
    let follow = |who: &str, login: &str| {
        w.s.follow_user(req(
            &w.tok(who),
            FollowUserRequest {
                login: login.into(),
            },
        ))
    };
    assert_eq!(
        reason(follow("reader", "reader-login").await),
        "INVALID_STATE"
    );
    assert_eq!(reason(follow("reader", "nobody").await), "NOT_FOUND");
    follow("reader", "writer-login").await.unwrap();
    follow("reader", "writer-login").await.unwrap(); // idempotent
    w.clock.advance(10);
    follow("outsider", "writer-login").await.unwrap();
    follow("writer", "reader-login").await.unwrap();
    let wp = user(&w, "reader", "writer-login").await.unwrap();
    assert_eq!(
        (
            wp.follower_count,
            wp.following_count,
            wp.viewer_is_following,
            wp.follows_viewer
        ),
        (2, 1, true, true)
    );
    let followers =
        w.s.list_user_followers(req(
            &w.tok("admin"),
            ListUserFollowersRequest {
                login: "writer-login".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        followers
            .users
            .iter()
            .map(|u| u.login.as_str())
            .collect::<Vec<_>>(),
        vec!["outsider-login", "reader-login"]
    );
    let following =
        w.s.list_user_following(req(&w.tok("reader"), ListUserFollowingRequest::default()))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(following.users.len(), 1);
    assert!(
        w.s.is_following_user(req(
            &w.tok("reader"),
            IsFollowingUserRequest {
                login: String::new(),
                target_login: "writer-login".into()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .following
    );
    assert!(
        !w.s.is_following_user(req(
            &w.tok("reader"),
            IsFollowingUserRequest {
                login: "writer-login".into(),
                target_login: "outsider-login".into()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .following
    );
    w.s.unfollow_user(req(
        &w.tok("outsider"),
        UnfollowUserRequest {
            login: "writer-login".into(),
        },
    ))
    .await
    .unwrap();
    w.s.unfollow_user(req(
        &w.tok("outsider"),
        UnfollowUserRequest {
            login: "writer-login".into(),
        },
    ))
    .await
    .unwrap(); // idempotent
    assert_eq!(
        user(&w, "reader", "writer-login")
            .await
            .unwrap()
            .follower_count,
        1
    );
    // Stars need read access; counts and stargazers follow.
    let star = |who: &str, repo: &str| {
        w.s.star_repo(req(&w.tok(who), StarRepoRequest { repo: rref(repo) }))
    };
    assert_eq!(reason(star("outsider", "acme/app").await), "NOT_FOUND");
    star("outsider", "acme/open").await.unwrap();
    star("outsider", "acme/open").await.unwrap();
    star("reader", "acme/open").await.unwrap();
    star("reader", "acme/app").await.unwrap();
    let gazers =
        w.s.list_repo_stargazers(req(
            &w.tok("writer"),
            ListRepoStargazersRequest {
                repo: rref("acme/open"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(gazers.total_count, 2);
    assert_eq!(
        gazers
            .stargazers
            .iter()
            .map(|g| g.user.as_ref().unwrap().login.as_str())
            .collect::<Vec<_>>()
            .len(),
        2
    );
    assert_eq!(
        user(&w, "writer", "reader-login").await.unwrap().star_count,
        2
    );
    // The starred list hides what the viewer cannot read.
    let starred = |who: &str, login: &str| {
        w.s.list_starred_repos(req(
            &w.tok(who),
            ListStarredReposRequest {
                login: login.into(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        starred("reader", "")
            .await
            .unwrap()
            .into_inner()
            .repos
            .len(),
        2
    );
    let seen = starred("outsider", "reader-login")
        .await
        .unwrap()
        .into_inner()
        .repos;
    assert_eq!(
        seen.iter()
            .map(|r| r.full_name.as_str())
            .collect::<Vec<_>>(),
        vec!["acme/open"]
    );
    assert_eq!(seen[0].star_count, 2);
    // Star lists: private ones are for the owner only; adding a repo stars it.
    let mk = |who: &str, name: &str, private: bool| {
        w.s.create_star_list(req(
            &w.tok(who),
            CreateStarListRequest {
                name: name.into(),
                description: "d".into(),
                private,
            },
        ))
    };
    let public = mk("reader", "To read", false).await.unwrap().into_inner();
    let private = mk("reader", "Secret", true).await.unwrap().into_inner();
    assert_eq!(
        reason(mk("reader", "to read", false).await),
        "ALREADY_EXISTS"
    );
    assert_eq!(reason(mk("reader", "", false).await), "INVALID_ARGUMENT");
    let add = |who: &str, list: &str, repo: &str| {
        w.s.add_star_list_repo(req(
            &w.tok(who),
            AddStarListRepoRequest {
                star_list_id: list.into(),
                repo: rref(repo),
            },
        ))
    };
    assert_eq!(
        add("reader", &public.id, "acme/open")
            .await
            .unwrap()
            .into_inner()
            .repo_count,
        1
    );
    add("reader", &public.id, "acme/open").await.unwrap(); // idempotent
    assert_eq!(
        reason(add("writer", &public.id, "acme/open").await),
        "NOT_FOUND"
    );
    add("reader", &private.id, "acme/app").await.unwrap();
    // A repo that is not starred yet is starred by adding it.
    add("reader", &public.id, "acme/open").await.unwrap();
    let is =
        w.s.is_repo_starred(req(
            &w.tok("reader"),
            IsRepoStarredRequest {
                repo: rref("acme/open"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(is.starred && is.star_list_ids == vec![public.id.clone()]);
    let lists = |who: &str, login: &str| {
        w.s.list_star_lists(req(
            &w.tok(who),
            ListStarListsRequest {
                login: login.into(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        lists("reader", "")
            .await
            .unwrap()
            .into_inner()
            .star_lists
            .len(),
        2
    );
    assert_eq!(
        lists("outsider", "reader-login")
            .await
            .unwrap()
            .into_inner()
            .star_lists
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        vec!["To read"]
    );
    assert_eq!(
        reason(
            w.s.get_star_list(req(
                &w.tok("outsider"),
                GetStarListRequest {
                    star_list_id: private.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(
            w.s.list_star_list_repos(req(
                &w.tok("outsider"),
                ListStarListReposRequest {
                    star_list_id: private.id.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let repos =
        w.s.list_star_list_repos(req(
            &w.tok("outsider"),
            ListStarListReposRequest {
                star_list_id: public.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .repos;
    assert_eq!(repos.len(), 1);
    let upd =
        w.s.update_star_list(req(
            &w.tok("reader"),
            UpdateStarListRequest {
                star_list_id: public.id.clone(),
                name: Some("Reading".into()),
                private: Some(true),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(upd.private && upd.name == "Reading");
    assert_eq!(
        reason(
            w.s.update_star_list(req(
                &w.tok("writer"),
                UpdateStarListRequest {
                    star_list_id: public.id.clone(),
                    name: Some("x".into()),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // Unstarring removes the repo from every list.
    w.s.unstar_repo(req(
        &w.tok("reader"),
        UnstarRepoRequest {
            repo: rref("acme/open"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        w.s.get_star_list(req(
            &w.tok("reader"),
            GetStarListRequest {
                star_list_id: public.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .repo_count,
        0
    );
    w.s.remove_star_list_repo(req(
        &w.tok("reader"),
        RemoveStarListRepoRequest {
            star_list_id: private.id.clone(),
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    w.s.delete_star_list(req(
        &w.tok("reader"),
        DeleteStarListRequest {
            star_list_id: private.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        lists("reader", "")
            .await
            .unwrap()
            .into_inner()
            .star_lists
            .len(),
        1
    );
    // Events: the repo's log carries star events for webhooks.
    w.flush().await;
    let kinds = w.event_kinds().await;
    assert!(
        kinds.iter().filter(|k| *k == "star.added").count() >= 3
            && kinds.contains(&"star.removed".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn emails_noreply_addresses_and_commit_attribution() {
    let w = world().await;
    let users = &w.env.forge.community.users;
    let sys = Cx::system();
    let add = |who: &str, email: &str| {
        w.s.add_user_email(req(
            &w.tok(who),
            AddUserEmailRequest {
                email: email.into(),
            },
        ))
    };
    assert_eq!(
        reason(add("reader", "not-an-email").await),
        "INVALID_ARGUMENT"
    );
    let e = add("reader", "Rita@Example.com")
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (e.email.as_str(), e.verified, e.primary),
        ("rita@example.com", false, false)
    );
    assert_eq!(
        reason(add("reader", "rita@example.com").await),
        "ALREADY_EXISTS"
    );
    // Unverified addresses cannot be primary or public.
    assert_eq!(
        reason(
            w.s.set_primary_user_email(req(
                &w.tok("reader"),
                SetPrimaryUserEmailRequest {
                    email: "rita@example.com".into()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    assert_eq!(
        reason(
            w.s.update_user_profile(req(
                &w.tok("reader"),
                UpdateUserProfileRequest {
                    public_email: Some("rita@example.com".into()),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    // Verification: the mailer takes the token; redeeming it verifies and makes it primary.
    let t = users.claim_verifications(&sys, 10).await.unwrap();
    assert_eq!(t.len(), 1);
    assert!(
        users
            .claim_verifications(&sys, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let verified = users.verify_email(&t[0].token).await.unwrap();
    assert!(verified.verified && verified.primary);
    assert_eq!(
        users.verify_email(&t[0].token).await.unwrap_err().reason,
        bee_proto::v1::ErrorReason::NotFound
    );
    // Another account cannot claim a verified address; it can add it unverified only if free.
    assert_eq!(
        reason(add("writer", "rita@example.com").await),
        "ALREADY_EXISTS"
    );
    assert_eq!(
        reason(add("writer", "someone@users.noreply.beehive.invalid").await),
        "INVALID_ARGUMENT"
    );
    let list =
        w.s.list_user_emails(req(&w.tok("reader"), ListUserEmailsRequest {}))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(
        list.noreply_address,
        "reader+reader-login@users.noreply.beehive.invalid"
    );
    assert!(!list.keep_private && !list.block_private_pushes);
    // A second address, verified, can become primary; the old primary can then be removed.
    add("reader", "work@example.com").await.unwrap();
    let t2 = users.claim_verifications(&sys, 10).await.unwrap();
    users.verify_email(&t2[0].token).await.unwrap();
    assert!(
        !w.s.list_user_emails(req(&w.tok("reader"), ListUserEmailsRequest {}))
            .await
            .unwrap()
            .into_inner()
            .emails
            .iter()
            .find(|e| e.email == "work@example.com")
            .unwrap()
            .primary
    );
    assert_eq!(
        reason(
            w.s.delete_user_email(req(
                &w.tok("reader"),
                DeleteUserEmailRequest {
                    email: "rita@example.com".into()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    assert!(
        w.s.set_primary_user_email(req(
            &w.tok("reader"),
            SetPrimaryUserEmailRequest {
                email: "work@example.com".into()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .primary
    );
    w.s.delete_user_email(req(
        &w.tok("reader"),
        DeleteUserEmailRequest {
            email: "rita@example.com".into(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.resend_user_email_verification(req(
                &w.tok("reader"),
                ResendUserEmailVerificationRequest {
                    email: "work@example.com".into()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Commit attribution: verified emails and the noreply address map to the account;
    // imported identities map to placeholders until the real owner verifies the address.
    let ghost = Principal {
        id: "placeholder-ann".into(),
        kind: PrincipalKind::Human as i32,
        display: "ann (imported)".into(),
        ..Default::default()
    };
    users
        .register_placeholder(&sys, "ann@old.example", &ghost)
        .await
        .unwrap();
    assert_eq!(
        users
            .register_placeholder(&sys, "work@example.com", &ghost)
            .await
            .unwrap_err()
            .reason,
        bee_proto::v1::ErrorReason::AlreadyExists
    );
    let resolve = |emails: &[&str]| {
        w.s.resolve_user_commit_emails(req(
            &w.tok("outsider"),
            ResolveUserCommitEmailsRequest {
                emails: emails.iter().map(|s| s.to_string()).collect(),
            },
        ))
    };
    let m = resolve(&[
        "work@example.com",
        "READER+reader-login@users.noreply.beehive.invalid",
        "ann@old.example",
        "who@nowhere.example",
    ])
    .await
    .unwrap()
    .into_inner()
    .mappings;
    assert_eq!(m[0].principal.as_ref().unwrap().id, "reader");
    assert_eq!(m[1].principal.as_ref().unwrap().id, "reader");
    assert_eq!(
        (
            m[2].placeholder,
            m[2].principal.as_ref().unwrap().id.as_str()
        ),
        (true, "placeholder-ann")
    );
    assert!(m[3].principal.is_none() && !m[3].placeholder);
    assert_eq!(reason(resolve(&vec!["x@y.z"; 101]).await), "TOO_LARGE");
    // The real Ann verifies the address and claims the identity.
    add("writer", "ann@old.example").await.unwrap();
    let t3 = users.claim_verifications(&sys, 10).await.unwrap();
    users.verify_email(&t3[0].token).await.unwrap();
    let m = resolve(&["ann@old.example"])
        .await
        .unwrap()
        .into_inner()
        .mappings;
    assert_eq!(
        (
            m[0].placeholder,
            m[0].principal.as_ref().unwrap().id.as_str()
        ),
        (false, "writer")
    );
    // Privacy: with pushes blocked, a private verified address is refused; public and noreply pass.
    let cx = w.cx("reader");
    assert!(
        users
            .commit_email_allowed(&cx, "reader", "work@example.com")
            .await
            .unwrap()
    );
    w.s.update_user_email_visibility(req(
        &w.tok("reader"),
        UpdateUserEmailVisibilityRequest {
            keep_private: Some(true),
            block_private_pushes: Some(true),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert!(
        !users
            .commit_email_allowed(&cx, "reader", "work@example.com")
            .await
            .unwrap()
    );
    assert!(
        users
            .commit_email_allowed(
                &cx,
                "reader",
                "reader+reader-login@users.noreply.beehive.invalid"
            )
            .await
            .unwrap()
    );
    assert!(
        users
            .commit_email_allowed(&cx, "reader", "elsewhere@example.com")
            .await
            .unwrap()
    );
    let vis =
        w.s.update_user_email_visibility(req(
            &w.tok("reader"),
            UpdateUserEmailVisibilityRequest {
                public_emails: vec!["work@example.com".into()],
                replace_public_emails: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(vis.keep_private && vis.emails.iter().any(|e| e.public));
    assert!(
        users
            .commit_email_allowed(&cx, "reader", "work@example.com")
            .await
            .unwrap()
    );
    assert_eq!(
        user(&w, "outsider", "reader-login")
            .await
            .unwrap()
            .public_email,
        "work@example.com"
    );
    // The security log records what happened, filterable by prefix.
    let log =
        w.s.list_user_security_events(req(
            &w.tok("reader"),
            ListUserSecurityEventsRequest {
                action_prefix: "email.".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let actions: Vec<&str> = log.events.iter().map(|e| e.action.as_str()).collect();
    assert!(
        actions.contains(&"email.add")
            && actions.contains(&"email.verify")
            && actions.contains(&"email.primary")
    );
    assert!(
        w.s.list_user_security_events(req(
            &w.tok("reader"),
            ListUserSecurityEventsRequest {
                action_prefix: "login.".into(),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .events
        .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_renames_settings_exports_and_account_deletion() {
    let w = world().await;
    let users = &w.env.forge.community.users;
    let cx = Cx::system();
    let rename = |who: &str, login: &str| {
        w.s.rename_user_login(req(
            &w.tok(who),
            RenameUserLoginRequest {
                new_login: login.into(),
            },
        ))
    };
    assert_eq!(
        reason(rename("reader", "bad_login").await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(reason(rename("reader", "acme").await), "ALREADY_EXISTS"); // an org holds it
    assert_eq!(
        reason(rename("reader", "owner-login").await),
        "ALREADY_EXISTS"
    ); // another user holds it
    let renamed = rename("reader", "rita").await.unwrap().into_inner();
    assert_eq!(renamed.login, "rita");
    // The old name redirects, case-insensitively.
    assert_eq!(
        user(&w, "outsider", "reader-login").await.unwrap().login,
        "rita"
    );
    assert_eq!(
        user(&w, "outsider", "RITA")
            .await
            .unwrap()
            .principal
            .unwrap()
            .id,
        "reader"
    );
    assert_eq!(reason(rename("writer", "rita").await), "ALREADY_EXISTS"); // a current login
    // While a name redirects nobody else can take it, but its owner can take it back.
    rename("reader", "rita2").await.unwrap();
    assert_eq!(user(&w, "outsider", "rita").await.unwrap().login, "rita2");
    assert_eq!(reason(rename("writer", "rita").await), "ALREADY_EXISTS");
    assert_eq!(
        rename("reader", "rita").await.unwrap().into_inner().login,
        "rita"
    );
    rename("reader", "rita3").await.unwrap();
    // After the redirect period the old name is free and purged.
    w.clock.advance(91 * DAY);
    users.purge_expired(&cx).await.unwrap();
    assert_eq!(user(&w, "outsider", "rita").await.unwrap_err(), "NOT_FOUND");
    assert_eq!(
        rename("writer", "rita").await.unwrap().into_inner().login,
        "rita"
    );
    // Settings validate and merge.
    let defaults =
        w.s.get_user_settings(req(&w.tok("admin"), GetUserSettingsRequest {}))
            .await
            .unwrap()
            .into_inner();
    assert_eq!(
        (
            defaults.theme.as_str(),
            defaults.time_zone.as_str(),
            defaults.diff_view.as_str()
        ),
        ("system", "UTC", "unified")
    );
    let set = |f: &dyn Fn(&mut UpdateUserSettingsRequest)| {
        let mut r = UpdateUserSettingsRequest::default();
        f(&mut r);
        w.s.update_user_settings(req(&w.tok("admin"), r))
    };
    assert_eq!(
        reason(set(&|r| r.time_zone = Some("Mars/Olympus".into())).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(set(&|r| r.locale = Some("not a locale".into())).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(set(&|r| r.editor_indent = Some("spaces:99".into())).await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(set(&|r| r.diff_view = Some("inline".into())).await),
        "INVALID_ARGUMENT"
    );
    let s1 = set(&|r| {
        r.theme = Some("dark".into());
        r.time_zone = Some("Europe/Berlin".into());
        r.locale = Some("de-DE".into());
        r.editor_indent = Some("tabs:4".into());
        r.diff_view = Some("split".into());
        r.vigilant_mode = Some(true);
        r.accessibility = vec![
            KeyValue {
                key: "reduce_motion".into(),
                value: "true".into(),
            },
            KeyValue {
                key: "contrast".into(),
                value: "high".into(),
            },
        ];
        r.keyboard_shortcuts = vec![KeyValue {
            key: "search".into(),
            value: "ctrl+k".into(),
        }];
    })
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        (
            s1.theme.as_str(),
            s1.time_zone.as_str(),
            s1.vigilant_mode,
            s1.accessibility.len()
        ),
        ("dark", "Europe/Berlin", true, 2)
    );
    let s2 = set(&|r| {
        r.accessibility = vec![KeyValue {
            key: "contrast".into(),
            value: String::new(),
        }]
    })
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        s2.accessibility
            .iter()
            .map(|k| k.key.as_str())
            .collect::<Vec<_>>(),
        vec!["reduce_motion"]
    );
    assert_eq!(s2.keyboard_shortcuts.len(), 1);
    // Data export: one pending at a time; the worker completes it; the link expires after a week.
    let ex =
        w.s.request_user_data_export(req(
            &w.tok("admin"),
            RequestUserDataExportRequest {
                include_repos: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ex.state, UserDataExportState::Pending as i32);
    assert_eq!(
        reason(
            w.s.request_user_data_export(req(
                &w.tok("admin"),
                RequestUserDataExportRequest::default()
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    let claimed = users.claim_exports(&cx, 10).await.unwrap();
    assert_eq!(
        claimed,
        vec![("admin".to_string(), ex.id.clone(), true, false)]
    );
    assert_eq!(
        w.s.get_user_data_export(req(
            &w.tok("admin"),
            GetUserDataExportRequest {
                export_id: ex.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .state,
        UserDataExportState::Running as i32
    );
    users
        .complete_export("admin", &ex.id, Ok((4096, vec![9; 32])))
        .await
        .unwrap();
    let ready =
        w.s.get_user_data_export(req(
            &w.tok("admin"),
            GetUserDataExportRequest {
                export_id: ex.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (ready.state, ready.size),
        (UserDataExportState::Ready as i32, 4096)
    );
    assert!(ready.download_url.contains("exports/admin/") && ready.url_expires_at.is_some());
    assert_eq!(
        reason(
            w.s.get_user_data_export(req(
                &w.tok("reader"),
                GetUserDataExportRequest {
                    export_id: ex.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    w.clock.advance(8 * DAY);
    assert_eq!(
        w.s.get_user_data_export(req(
            &w.tok("admin"),
            GetUserDataExportRequest { export_id: ex.id }
        ))
        .await
        .unwrap()
        .into_inner()
        .state,
        UserDataExportState::Expired as i32
    );
    // Deleting an account: confirmation, and not as the only owner of an org.
    let del = |who: &str, confirm: &str| {
        w.s.delete_user_account(req(
            &w.tok(who),
            DeleteUserAccountRequest {
                confirm_login: confirm.into(),
                reason: "leaving".into(),
            },
        ))
    };
    assert_eq!(reason(del("outsider", "wrong").await), "INVALID_ARGUMENT");
    assert_eq!(reason(del("owner", "owner-login").await), "INVALID_STATE");
    let gone = del("outsider", "outsider-login")
        .await
        .unwrap()
        .into_inner();
    assert!(gone.purge_after.is_some());
    assert_eq!(
        user(&w, "reader", "outsider-login").await.unwrap_err(),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(del("outsider", "outsider-login").await),
        "INVALID_STATE"
    );
    // Support can restore before the purge time, not after.
    users.restore_account(&cx, "outsider").await.unwrap();
    assert!(user(&w, "reader", "outsider-login").await.is_ok());
    del("outsider", "outsider-login").await.unwrap();
    w.clock.advance(91 * DAY);
    assert_eq!(
        users
            .restore_account(&cx, "outsider")
            .await
            .unwrap_err()
            .reason,
        bee_proto::v1::ErrorReason::InvalidState
    );
    assert_eq!(users.purge_expired(&cx).await.unwrap(), 1);
    // Events: renames and deletions are announced.
    w.flush().await;
    let (events, _) = w
        .env
        .forge
        .events
        .read(&cx, "system", 0, &[], 1000)
        .await
        .unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    for k in [
        "user.login_renamed",
        "user.settings_updated",
        "user.export_requested",
        "user.deleted",
        "user.restored",
    ] {
        assert!(kinds.contains(&k), "missing {k}");
    }
}
