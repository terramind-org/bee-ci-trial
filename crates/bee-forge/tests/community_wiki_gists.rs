//! `WikiService` (a sibling `<repo>.wiki` repo in the VCS) and `GistService` (user-owned
//! repos): permissions, history, optimistic concurrency, forks, stars and comments.

use crate::community_support as support;

use bee_forge::community::ports::ContentVcs;
use bee_policy::Action;
use bee_proto::v1::gist_service_server::GistService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::wiki_service_server::WikiService;
use bee_proto::v1::*;
use support::*;

async fn enable(w: &World, who: &str, access: WikiEditAccess) -> Result<WikiInfo, String> {
    w.s.enable_wiki(req(
        &w.tok(who),
        EnableWikiRequest {
            repo: rref("acme/app"),
            edit_access: access as i32,
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|st| reason::<()>(Err(st)))
}

async fn page(w: &World, who: &str, title: &str, content: &str) -> Result<WikiPage, String> {
    w.s.create_wiki_page(req(
        &w.tok(who),
        CreateWikiPageRequest {
            repo: rref("acme/app"),
            title: title.into(),
            content: content.into(),
            ..Default::default()
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|st| reason::<()>(Err(st)))
}

async fn get(w: &World, who: &str, slug: &str) -> Result<WikiPage, String> {
    w.s.get_wiki_page(req(
        &w.tok(who),
        GetWikiPageRequest {
            repo: rref("acme/app"),
            slug: slug.into(),
            ..Default::default()
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|st| reason::<()>(Err(st)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wiki_pages_live_in_a_sibling_repo_with_history() {
    let w = world().await;
    // Off until a maintainer turns it on.
    assert_eq!(
        page(&w, "writer", "Home", "hi").await.unwrap_err(),
        "INVALID_STATE"
    );
    assert_eq!(
        enable(&w, "writer", WikiEditAccess::Collaborators)
            .await
            .unwrap_err(),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        enable(&w, "outsider", WikiEditAccess::Collaborators)
            .await
            .unwrap_err(),
        "NOT_FOUND"
    );
    let info = enable(&w, "admin", WikiEditAccess::Collaborators)
        .await
        .unwrap();
    assert!(info.enabled);
    assert!(info.clone_url_https.ends_with("acme/app.wiki.git"));
    match info.wiki_repo.unwrap().repo.unwrap() {
        repo_ref::Repo::FullName(n) => assert_eq!(n, "acme/app.wiki"),
        other => panic!("{other:?}"),
    }
    // Collaborators edit; plain readers do not; strangers do not even see the repo.
    assert_eq!(
        page(&w, "reader", "Home", "hi").await.unwrap_err(),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        page(&w, "outsider", "Home", "hi").await.unwrap_err(),
        "NOT_FOUND"
    );
    let home = page(&w, "writer", "Home", "# Welcome\nSee [[Getting Started]].")
        .await
        .unwrap();
    assert_eq!((home.slug.as_str(), home.title.as_str()), ("Home", "Home"));
    assert_eq!(home.last_editor.as_ref().unwrap().id, "writer");
    let gs = page(
        &w,
        "writer",
        "Getting Started",
        "Run `bee land` to land a CR.",
    )
    .await
    .unwrap();
    assert_eq!(gs.slug, "Getting-Started");
    assert_eq!(gs.path, b"Getting-Started.md".to_vec());
    assert_eq!(
        page(&w, "writer", "Getting Started", "again")
            .await
            .unwrap_err(),
        "ALREADY_EXISTS"
    );
    assert_eq!(
        page(&w, "writer", "a/b", "x").await.unwrap_err(),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        page(&w, "writer", "_Sidebar", "x").await.unwrap_err(),
        "INVALID_ARGUMENT"
    );
    // The pages are files of the sibling repo.
    let vr = bee_forge::community::ports::VcsRepo::wiki_of(&w.app);
    let head = w.vcs.head(&vr).await.unwrap().unwrap();
    let files: Vec<String> = w
        .vcs
        .tree(&vr, &head)
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert_eq!(files, vec!["Getting-Started.md", "Home.md"]);
    // Readers read, with paging and a listing sorted by slug.
    let list =
        w.s.list_wiki_pages(req(
            &w.tok("reader"),
            ListWikiPagesRequest {
                repo: rref("acme/app"),
                page: Some(PageRequest {
                    page_size: 1,
                    page_token: String::new(),
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.pages.len(), 1);
    assert_eq!(list.pages[0].slug, "Getting-Started");
    let rest =
        w.s.list_wiki_pages(req(
            &w.tok("reader"),
            ListWikiPagesRequest {
                repo: rref("acme/app"),
                page: Some(PageRequest {
                    page_size: 5,
                    page_token: list.page.unwrap().next_page_token,
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(rest.pages[0].slug, "Home");
    assert_eq!(
        w.s.get_wiki(req(
            &w.tok("reader"),
            GetWikiRequest {
                repo: rref("acme/app")
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .page_count,
        2
    );
    // Optimistic concurrency: an edit based on a stale revision is refused.
    let update =
        |who: &str, slug: &str, expected: Option<ObjectId>, content: &str, title: Option<&str>| {
            w.s.update_wiki_page(req(
                &w.tok(who),
                UpdateWikiPageRequest {
                    repo: rref("acme/app"),
                    slug: slug.into(),
                    expected_changeset: expected,
                    content: Some(content.into()),
                    title: title.map(String::from),
                    ..Default::default()
                },
            ))
        };
    let v2 = update(
        "writer",
        "Home",
        home.changeset.clone(),
        "# Welcome\nUpdated.",
        None,
    )
    .await
    .unwrap()
    .into_inner();
    assert_ne!(v2.changeset, home.changeset);
    assert_eq!(
        reason(update("agent", "Home", home.changeset.clone(), "stale", None).await),
        "VERSION_MISMATCH"
    );
    assert_eq!(
        get(&w, "reader", "Home").await.unwrap().content,
        "# Welcome\nUpdated."
    );
    // Rename keeps the content under the new slug.
    let renamed = update(
        "writer",
        "Getting-Started",
        None,
        "Run `bee land`.",
        Some("Quick Start"),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(renamed.slug, "Quick-Start");
    assert_eq!(
        get(&w, "reader", "Getting-Started").await.unwrap_err(),
        "NOT_FOUND"
    );
    assert_eq!(
        page(&w, "writer", "Home", "dup").await.unwrap_err(),
        "ALREADY_EXISTS"
    );
    assert_eq!(
        reason(update("writer", "Quick-Start", None, "x", Some("Home")).await),
        "ALREADY_EXISTS"
    );
    // History: newest first, authored by whoever edited.
    let revs =
        w.s.list_wiki_page_revisions(req(
            &w.tok("reader"),
            ListWikiPageRevisionsRequest {
                repo: rref("acme/app"),
                slug: "Home".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(revs.revisions.len(), 2);
    assert_eq!(revs.revisions[0].message, "Update Home");
    assert_eq!(revs.revisions[1].message, "Create Home");
    // Sidebar and footer are special pages that are never listed.
    let set = |content: &str| {
        w.s.set_wiki_sidebar(req(
            &w.tok("writer"),
            SetWikiSidebarRequest {
                repo: rref("acme/app"),
                content: content.into(),
                ..Default::default()
            },
        ))
    };
    set("* [Home](Home)").await.unwrap();
    assert_eq!(
        w.s.get_wiki_sidebar(req(
            &w.tok("reader"),
            GetWikiSidebarRequest {
                repo: rref("acme/app"),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .content,
        "* [Home](Home)"
    );
    w.s.set_wiki_footer(req(
        &w.tok("writer"),
        SetWikiFooterRequest {
            repo: rref("acme/app"),
            content: "(c) acme".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        w.s.list_wiki_pages(req(
            &w.tok("reader"),
            ListWikiPagesRequest {
                repo: rref("acme/app"),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .pages
        .len(),
        2
    );
    set("").await.unwrap();
    assert_eq!(
        reason(
            w.s.get_wiki_sidebar(req(
                &w.tok("reader"),
                GetWikiSidebarRequest {
                    repo: rref("acme/app"),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // Search ranks by occurrences with the title weighted, and escapes snippets.
    page(
        &w,
        "writer",
        "Land",
        "Landing *safely* needs leases.\nland land",
    )
    .await
    .unwrap();
    let hits =
        w.s.search_wiki_pages(req(
            &w.tok("reader"),
            SearchWikiPagesRequest {
                repo: rref("acme/app"),
                query: "land".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .hits;
    assert_eq!(hits[0].page.as_ref().unwrap().slug, "Land");
    assert!(hits[0].snippets[0].contains("\\*safely\\*"));
    assert!(
        hits.iter()
            .any(|h| h.page.as_ref().unwrap().slug == "Quick-Start")
    );
    // Deleting returns the removing changeset.
    let del =
        w.s.delete_wiki_page(req(
            &w.tok("writer"),
            DeleteWikiPageRequest {
                repo: rref("acme/app"),
                slug: "Land".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(del.changeset.is_some());
    assert_eq!(get(&w, "reader", "Land").await.unwrap_err(), "NOT_FOUND");
    // Events.
    w.flush().await;
    let kinds = w.event_kinds().await;
    assert!(kinds.contains(&"wiki.enabled".to_string()));
    assert!(kinds.iter().filter(|k| *k == "wiki.page_updated").count() >= 6);
    // Disabling keeps the repo but stops page calls; archived repos are read-only.
    w.s.disable_wiki(req(
        &w.tok("admin"),
        DisableWikiRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        get(&w, "reader", "Home").await.unwrap_err(),
        "INVALID_STATE"
    );
    assert!(w.vcs.head(&vr).await.unwrap().is_some());
    enable(&w, "admin", WikiEditAccess::Collaborators)
        .await
        .unwrap();
    assert_eq!(get(&w, "reader", "Home").await.unwrap().slug, "Home");
    w.s.archive_repo(req(
        &w.tok("owner"),
        ArchiveRepoRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        page(&w, "writer", "Late", "x").await.unwrap_err(),
        "ARCHIVED"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_wikis_let_every_reader_edit() {
    let w = world().await;
    let open = |who: &str, title: &str| {
        w.s.create_wiki_page(req(
            &w.tok(who),
            CreateWikiPageRequest {
                repo: rref("acme/open"),
                title: title.into(),
                content: "x".into(),
                ..Default::default()
            },
        ))
    };
    w.s.enable_wiki(req(
        &w.tok("admin"),
        EnableWikiRequest {
            repo: rref("acme/open"),
            edit_access: WikiEditAccess::Readers as i32,
        },
    ))
    .await
    .unwrap();
    // Even someone outside the org, on a public repo, may add a page.
    assert!(open("outsider", "Community Notes").await.is_ok());
    assert!(open("reader", "More Notes").await.is_ok());
    // A token that cannot read cannot edit either.
    let no_read = w.env.token("reader", actions(&[Action::Triage]));
    assert_eq!(
        reason(
            w.s.create_wiki_page(req(
                &no_read,
                CreateWikiPageRequest {
                    repo: rref("acme/open"),
                    title: "x".into(),
                    content: "x".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "POLICY_DENIED"
    );
}

async fn new_gist(
    w: &World,
    who: &str,
    files: &[(&str, &str)],
    public: bool,
) -> Result<Gist, String> {
    w.s.create_gist(req(
        &w.tok(who),
        CreateGistRequest {
            files: files
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            description: "a gist".into(),
            public,
        },
    ))
    .await
    .map(|r| r.into_inner())
    .map_err(|st| reason::<()>(Err(st)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gists_have_visibility_revisions_and_owner_only_changes() {
    let w = world().await;
    let public = new_gist(
        &w,
        "owner",
        &[("hello.rs", "fn main() {}\n"), ("notes.md", "# notes\n")],
        true,
    )
    .await
    .unwrap();
    let secret = new_gist(&w, "owner", &[("key.txt", "s3cret\n")], false)
        .await
        .unwrap();
    assert_eq!(
        public
            .files
            .iter()
            .map(|f| f.filename.as_str())
            .collect::<Vec<_>>(),
        vec!["hello.rs", "notes.md"]
    );
    assert_eq!(public.files[0].language, "Rust");
    assert_eq!(public.files[0].content, "fn main() {}\n");
    assert!(public.clone_url_https.ends_with(".git") && public.embed_url.ends_with(".js"));
    assert_eq!(
        new_gist(&w, "owner", &[], true).await.unwrap_err(),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        new_gist(&w, "owner", &[("a/b", "x")], true)
            .await
            .unwrap_err(),
        "INVALID_ARGUMENT"
    );
    // Public gists are listed for everyone; secret ones are not, but anyone with the id reads them.
    let listed = |who: &str, filter: GistListFilter, user: &str| {
        w.s.list_gists(req(
            &w.tok(who),
            ListGistsRequest {
                filter: filter as i32,
                user: user.into(),
                ..Default::default()
            },
        ))
    };
    let public_ids: Vec<String> = listed("outsider", GistListFilter::Public, "")
        .await
        .unwrap()
        .into_inner()
        .gists
        .into_iter()
        .map(|g| g.id)
        .collect();
    assert_eq!(public_ids, vec![public.id.clone()]);
    assert!(
        listed("outsider", GistListFilter::Public, "")
            .await
            .unwrap()
            .into_inner()
            .gists[0]
            .files
            .iter()
            .all(|f| f.content.is_empty())
    );
    let mine = listed("owner", GistListFilter::Mine, "")
        .await
        .unwrap()
        .into_inner()
        .gists;
    assert_eq!(mine.len(), 2);
    let theirs = listed("reader", GistListFilter::User, "owner-login")
        .await
        .unwrap()
        .into_inner()
        .gists;
    assert_eq!(theirs.len(), 1);
    let got =
        w.s.get_gist(req(
            &w.tok("outsider"),
            GetGistRequest {
                gist_id: secret.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.files[0].content, "s3cret\n");
    assert!(!got.public);
    // Truncation of large files.
    let big = "x".repeat(5000);
    let g = new_gist(&w, "owner", &[("big.txt", &big)], true)
        .await
        .unwrap();
    let cut =
        w.s.get_gist(req(
            &w.tok("reader"),
            GetGistRequest {
                gist_id: g.id.clone(),
                max_inline_bytes: 100,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        cut.files[0].truncated && cut.files[0].content.len() == 100 && cut.files[0].size == 5000
    );
    // Owner-only updates, one changeset per update, optimistic on the head.
    let update = |who: &str, id: &str, head: Option<ObjectId>, changes: Vec<GistFileChange>| {
        w.s.update_gist(req(
            &w.tok(who),
            UpdateGistRequest {
                gist_id: id.into(),
                expected_head: head,
                file_changes: changes,
                ..Default::default()
            },
        ))
    };
    let edit = |name: &str, content: &str| GistFileChange {
        filename: name.into(),
        content: Some(content.into()),
        ..Default::default()
    };
    assert_eq!(
        reason(update("reader", &public.id, None, vec![edit("hello.rs", "pwned")]).await),
        "PERMISSION_DENIED"
    );
    let v2 = update(
        "owner",
        &public.id,
        public.head.clone(),
        vec![
            edit("hello.rs", "fn main() { println!(\"hi\"); }\n"),
            edit("new.txt", "new file"),
        ],
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(v2.files.len(), 3);
    assert_eq!(
        reason(
            update(
                "owner",
                &public.id,
                public.head.clone(),
                vec![edit("hello.rs", "stale")]
            )
            .await
        ),
        "VERSION_MISMATCH"
    );
    let v3 = update(
        "owner",
        &public.id,
        v2.head.clone(),
        vec![
            GistFileChange {
                filename: "new.txt".into(),
                new_filename: Some("renamed.txt".into()),
                ..Default::default()
            },
            GistFileChange {
                filename: "notes.md".into(),
                delete: true,
                ..Default::default()
            },
        ],
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        v3.files
            .iter()
            .map(|f| f.filename.as_str())
            .collect::<Vec<_>>(),
        vec!["hello.rs", "renamed.txt"]
    );
    assert_eq!(v3.files[1].content, "new file");
    assert_eq!(
        reason(
            update(
                "owner",
                &public.id,
                None,
                vec![GistFileChange {
                    filename: "missing".into(),
                    delete: true,
                    ..Default::default()
                }]
            )
            .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(
            update(
                "owner",
                &secret.id,
                None,
                vec![GistFileChange {
                    filename: "key.txt".into(),
                    delete: true,
                    ..Default::default()
                }]
            )
            .await
        ),
        "INVALID_ARGUMENT"
    );
    // Revisions, newest first, with line counts; old revisions stay readable.
    let revs =
        w.s.list_gist_revisions(req(
            &w.tok("reader"),
            ListGistRevisionsRequest {
                gist_id: public.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .revisions;
    assert_eq!(revs.len(), 3);
    assert_eq!(revs[0].changeset, v3.head);
    assert_eq!(revs[2].changeset, public.head);
    assert!(revs[1].additions >= 2);
    let old =
        w.s.get_gist_revision(req(
            &w.tok("reader"),
            GetGistRevisionRequest {
                gist_id: public.id.clone(),
                changeset: public.head.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(old.files.len(), 2);
    assert_eq!(old.files[0].content, "fn main() {}\n");
    let raw =
        w.s.get_gist_file_raw_url(req(
            &w.tok("reader"),
            GetGistFileRawUrlRequest {
                gist_id: public.id.clone(),
                filename: "hello.rs".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(raw.url.contains("signature=") && raw.size > 0 && raw.expires_at.is_some());
    // Delete: owner only; the repo goes with it.
    assert_eq!(
        reason(
            w.s.delete_gist(req(
                &w.tok("reader"),
                DeleteGistRequest {
                    gist_id: secret.id.clone()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    w.s.delete_gist(req(
        &w.tok("owner"),
        DeleteGistRequest {
            gist_id: secret.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.get_gist(req(
                &w.tok("owner"),
                GetGistRequest {
                    gist_id: secret.id.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert!(
        w.vcs
            .head(&bee_forge::community::ports::VcsRepo::gist(&secret.id))
            .await
            .unwrap()
            .is_none()
    );
    // Events.
    w.flush().await;
    let kinds: Vec<String> = {
        use bee_forge::Cx;
        let (events, _) = w
            .env
            .forge
            .events
            .read(&Cx::system(), "system", 0, &[], 1000)
            .await
            .unwrap();
        events.into_iter().map(|e| e.kind).collect()
    };
    for k in ["gist.created", "gist.updated", "gist.deleted"] {
        assert!(kinds.iter().any(|x| x == k), "missing {k}");
    }
}

async fn star(w: &World, id: &str, who: &str, on: bool) -> Result<(), tonic::Status> {
    let t = w.tok(who);
    if on {
        w.s.star_gist(req(&t, StarGistRequest { gist_id: id.into() }))
            .await
            .map(|_| ())
    } else {
        w.s.unstar_gist(req(&t, UnstarGistRequest { gist_id: id.into() }))
            .await
            .map(|_| ())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gist_forks_stars_and_comments() {
    let w = world().await;
    let g = new_gist(&w, "owner", &[("a.txt", "one")], true)
        .await
        .unwrap();
    // Forking your own gist is refused; others fork with history and the same visibility.
    assert_eq!(
        reason(
            w.s.fork_gist(req(
                &w.tok("owner"),
                ForkGistRequest {
                    gist_id: g.id.clone()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    let fork =
        w.s.fork_gist(req(
            &w.tok("reader"),
            ForkGistRequest {
                gist_id: g.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            fork.fork_of.as_str(),
            fork.owner.as_ref().unwrap().id.as_str(),
            fork.files[0].content.as_str()
        ),
        (g.id.as_str(), "reader", "one")
    );
    w.s.fork_gist(req(
        &w.tok("writer"),
        ForkGistRequest {
            gist_id: g.id.clone(),
        },
    ))
    .await
    .unwrap();
    let forks =
        w.s.list_gist_forks(req(
            &w.tok("outsider"),
            ListGistForksRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .forks;
    assert_eq!(forks.len(), 2);
    assert_eq!(
        w.s.get_gist(req(
            &w.tok("owner"),
            GetGistRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .fork_count,
        2
    );
    // The fork has its own history: editing it leaves the original alone.
    w.s.update_gist(req(
        &w.tok("reader"),
        UpdateGistRequest {
            gist_id: fork.id.clone(),
            file_changes: vec![GistFileChange {
                filename: "a.txt".into(),
                content: Some("changed".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        w.s.get_gist(req(
            &w.tok("owner"),
            GetGistRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .files[0]
            .content,
        "one"
    );
    // Stars are idempotent and counted; the starred list shows them.
    star(&w, &g.id, "reader", true).await.unwrap();
    star(&w, &g.id, "reader", true).await.unwrap();
    star(&w, &g.id, "writer", true).await.unwrap();
    assert!(
        w.s.is_gist_starred(req(
            &w.tok("reader"),
            IsGistStarredRequest {
                gist_id: g.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .starred
    );
    assert!(
        !w.s.is_gist_starred(req(
            &w.tok("outsider"),
            IsGistStarredRequest {
                gist_id: g.id.clone()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .starred
    );
    assert_eq!(
        w.s.get_gist(req(
            &w.tok("owner"),
            GetGistRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .star_count,
        2
    );
    let starred =
        w.s.list_gists(req(
            &w.tok("reader"),
            ListGistsRequest {
                filter: GistListFilter::Starred as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .gists;
    assert_eq!(starred.len(), 1);
    star(&w, &g.id, "reader", false).await.unwrap();
    assert_eq!(
        w.s.get_gist(req(
            &w.tok("owner"),
            GetGistRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .star_count,
        1
    );
    // Comments; the owner can lock them and still comment.
    let comment = |who: &str, body: &str| {
        w.s.create_gist_comment(req(
            &w.tok(who),
            CreateGistCommentRequest {
                gist_id: g.id.clone(),
                body: body.into(),
            },
        ))
    };
    comment("reader", "nice").await.unwrap();
    comment("owner", "thanks").await.unwrap();
    assert_eq!(reason(comment("reader", "").await), "INVALID_ARGUMENT");
    w.s.update_gist(req(
        &w.tok("owner"),
        UpdateGistRequest {
            gist_id: g.id.clone(),
            comments_locked: Some(true),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(reason(comment("reader", "more").await), "LOCKED");
    comment("owner", "closing").await.unwrap();
    let comments =
        w.s.list_gist_comments(req(
            &w.tok("outsider"),
            ListGistCommentsRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .comments;
    assert_eq!(
        comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(),
        vec!["nice", "thanks", "closing"]
    );
    assert_eq!(comments[1].author_association, "OWNER");
    // A token that may only read cannot change anything.
    let ro = w.env.token("owner", actions(&[Action::Read]));
    assert_eq!(
        reason(
            w.s.create_gist(req(
                &ro,
                CreateGistRequest {
                    files: [("x".to_string(), "y".to_string())].into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "POLICY_DENIED"
    );
    assert!(
        w.s.get_gist(req(
            &ro,
            GetGistRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            }
        ))
        .await
        .is_ok()
    );
    // Deleting a fork gives the parent its count back.
    w.s.delete_gist(req(
        &w.tok("reader"),
        DeleteGistRequest {
            gist_id: fork.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        w.s.get_gist(req(
            &w.tok("owner"),
            GetGistRequest {
                gist_id: g.id.clone(),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .fork_count,
        1
    );
}
