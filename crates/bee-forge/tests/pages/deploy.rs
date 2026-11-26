//! Sites, builds, deployments, serving and previews.

use bee_forge::Cx;
use bee_forge::auth::Caller;
use bee_forge::pages::Artifact;
use bee_proto::v1::pages_service_server::PagesService;
use bytes::Bytes;

use super::support::*;

pub async fn get(
    w: &World,
    host: &str,
    path: &str,
    caller: Option<&Caller>,
    https: bool,
) -> http::Response<Bytes> {
    let r = rq("GET", path, None, &[("host", host)], Bytes::new());
    w.env.forge.pages.respond(caller, &r, https).await
}

fn author() -> Principal {
    Principal {
        id: "writer".into(),
        kind: PrincipalKind::Human as i32,
        ..Default::default()
    }
}

async fn seed(w: &World, files: &[(&str, &str)], msg: &str) -> ObjectId {
    w.vcs
        .seed(&w.vr(&w.app), files, &author(), msg, T0)
        .await
        .unwrap()
}

pub async fn create_site(
    w: &World,
    build: PagesBuildType,
    path: &str,
    access: PagesAccess,
) -> PagesSite {
    w.s.create_pages_site(req(
        &w.tok("owner"),
        CreatePagesSiteRequest {
            repo: rref("acme/app"),
            build_type: build as i32,
            source: Some(PagesSource {
                r#ref: "main".into(),
                path: path.into(),
            }),
            access: access as i32,
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn build(w: &World) -> PagesDeployment {
    w.s.request_pages_build(req(
        &w.tok("writer"),
        RequestPagesBuildRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

const HOST: &str = "acme.pages.test";

#[tokio::test]
async fn site_permissions_and_policy() {
    let w = world().await;
    let mk = |who: &str| {
        let t = w.tok(who);
        let s = w.s.clone();
        async move {
            reason(
                s.create_pages_site(req(
                    &t,
                    CreatePagesSiteRequest {
                        repo: rref("acme/app"),
                        ..Default::default()
                    },
                ))
                .await,
            )
        }
    };
    assert_eq!(mk("reader").await, "PERMISSION_DENIED");
    assert_eq!(mk("writer").await, "PERMISSION_DENIED");
    assert_eq!(mk("outsider").await, "NOT_FOUND");
    // Reads need read access; the private repo hides the site from outsiders.
    let site = create_site(&w, PagesBuildType::Ref, "/", PagesAccess::Public).await;
    assert_eq!(site.url, "https://acme.pages.test/app/");
    assert_eq!(site.status, PagesSiteStatus::None as i32);
    assert_eq!(
        reason(
            w.s.create_pages_site(req(
                &w.tok("owner"),
                CreatePagesSiteRequest {
                    repo: rref("acme/app"),
                    ..Default::default()
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    assert!(
        w.s.get_pages_site(req(
            &w.tok("reader"),
            GetPagesSiteRequest {
                repo: rref("acme/app")
            }
        ))
        .await
        .is_ok()
    );
    assert_eq!(
        reason(
            w.s.get_pages_site(req(
                &w.tok("outsider"),
                GetPagesSiteRequest {
                    repo: rref("acme/app")
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // Updating needs admin.
    let upd = |who: &str| {
        let t = w.tok(who);
        let s = w.s.clone();
        async move {
            s.update_pages_site(req(
                &t,
                UpdatePagesSiteRequest {
                    repo: rref("acme/app"),
                    access: Some(PagesAccess::Private as i32),
                    ..Default::default()
                },
            ))
            .await
        }
    };
    assert_eq!(reason(upd("writer").await), "PERMISSION_DENIED");
    assert_eq!(
        upd("admin").await.unwrap().into_inner().access,
        PagesAccess::Private as i32
    );
    // The org forbids public sites.
    w.env
        .forge
        .pages
        .set_forbid_public(&Cx::system(), &w.app.org_id, true)
        .await
        .unwrap();
    let e =
        w.s.update_pages_site(req(
            &w.tok("owner"),
            UpdatePagesSiteRequest {
                repo: rref("acme/app"),
                access: Some(PagesAccess::Public as i32),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(e), "POLICY_DENIED");
    let e =
        w.s.create_pages_site(req(
            &w.tok("owner"),
            CreatePagesSiteRequest {
                repo: rref("acme/open"),
                access: PagesAccess::Public as i32,
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(reason(e), "POLICY_DENIED");
    // Denied changes are audited and delete removes the site.
    w.s.delete_pages_site(req(
        &w.tok("owner"),
        DeletePagesSiteRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            w.s.get_pages_site(req(
                &w.tok("owner"),
                GetPagesSiteRequest {
                    repo: rref("acme/app")
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
}

#[tokio::test]
async fn deploy_from_a_tree_serves_and_switches_atomically() {
    let w = world().await;
    create_site(&w, PagesBuildType::Ref, "/", PagesAccess::Public).await;
    // Nothing deployed yet.
    assert_eq!(get(&w, HOST, "/app/", None, true).await.status(), 404);
    seed(
        &w,
        &[
            ("index.html", "<h1>v1</h1>"),
            ("about.html", "about v1"),
            ("docs/index.html", "docs"),
            ("404.html", "custom 404"),
            ("css/site.css", "body{}"),
            ("_redirects", "/old /about 301\n"),
        ],
        "v1",
    )
    .await;
    let d1 = build(&w).await;
    assert_eq!(
        d1.state,
        PagesDeploymentState::Succeeded as i32,
        "{}",
        d1.error_message
    );
    assert_eq!(d1.page_url, "https://acme.pages.test/app/");
    let site =
        w.s.get_pages_site(req(
            &w.tok("owner"),
            GetPagesSiteRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(site.current_deployment_id, d1.id);
    assert_eq!(site.status, PagesSiteStatus::Built as i32);
    assert!(site.served_tree.is_some());
    // Serving rules.
    let r = get(&w, HOST, "/app/", None, true).await;
    assert_eq!(r.status(), 200);
    assert_eq!(body_str(&r), "<h1>v1</h1>");
    assert_eq!(hdr(&r, "content-type"), "text/html; charset=utf-8");
    assert_eq!(hdr(&r, "cache-control"), "max-age=600");
    assert_eq!(
        body_str(&get(&w, HOST, "/app/about", None, true).await),
        "about v1"
    );
    assert_eq!(get(&w, HOST, "/app/docs", None, true).await.status(), 301);
    assert_eq!(
        hdr(&get(&w, HOST, "/app/docs", None, true).await, "location"),
        "/app/docs/"
    );
    assert_eq!(
        body_str(&get(&w, HOST, "/app/docs/", None, true).await),
        "docs"
    );
    assert_eq!(
        hdr(
            &get(&w, HOST, "/app/css/site.css", None, true).await,
            "content-type"
        ),
        "text/css; charset=utf-8"
    );
    assert_eq!(
        hdr(&get(&w, HOST, "/app/old", None, true).await, "location"),
        "/app/about"
    );
    let nf = get(&w, HOST, "/app/missing", None, true).await;
    assert_eq!(
        (nf.status().as_u16(), body_str(&nf).as_str()),
        (404, "custom 404")
    );
    assert_eq!(get(&w, HOST, "/app", None, true).await.status(), 301);
    assert_eq!(get(&w, HOST, "/other/", None, true).await.status(), 404);
    assert_eq!(
        get(&w, "unknown.example.org", "/", None, true)
            .await
            .status(),
        404
    );
    // `_redirects` itself is not served.
    assert_eq!(
        get(&w, HOST, "/app/_redirects", None, true).await.status(),
        404
    );
    // Conditional requests.
    let etag = hdr(&get(&w, HOST, "/app/", None, true).await, "etag").to_string();
    let req304 = rq(
        "GET",
        "/app/",
        None,
        &[("host", HOST), ("if-none-match", &etag)],
        Bytes::new(),
    );
    assert_eq!(
        w.env
            .forge
            .pages
            .respond(None, &req304, true)
            .await
            .status(),
        304
    );
    let head = rq("HEAD", "/app/", None, &[("host", HOST)], Bytes::new());
    let h = w.env.forge.pages.respond(None, &head, true).await;
    assert_eq!((h.status().as_u16(), h.body().len()), (200, 0));
    // The ref has not moved: the same deployment comes back.
    assert_eq!(build(&w).await.id, d1.id);
    // New content switches atomically; a failed build leaves it serving.
    seed(&w, &[("index.html", "<h1>v2</h1>")], "v2").await;
    let d2 = build(&w).await;
    assert_ne!(d2.id, d1.id);
    assert_eq!(
        body_str(&get(&w, HOST, "/app/", None, true).await),
        "<h1>v2</h1>"
    );
    assert_eq!(
        body_str(&get(&w, HOST, "/app/about", None, true).await),
        "about v1"
    );
    w.vcs
        .set_ref(&w.vr(&w.app), "main", &ObjectId { raw: vec![9; 32] })
        .unwrap();
    let bad = build(&w).await;
    assert_eq!(bad.state, PagesDeploymentState::Failed as i32);
    assert!(!bad.error_message.is_empty());
    assert_eq!(
        body_str(&get(&w, HOST, "/app/", None, true).await),
        "<h1>v2</h1>"
    );
    let site =
        w.s.get_pages_site(req(
            &w.tok("owner"),
            GetPagesSiteRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(site.status, PagesSiteStatus::Errored as i32);
    assert_eq!(site.current_deployment_id, d2.id);
    // Deployment listing, newest first, and a finished deployment cannot be cancelled.
    let list =
        w.s.list_pages_deployments(req(
            &w.tok("reader"),
            ListPagesDeploymentsRequest {
                repo: rref("acme/app"),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.deployments.len(), 3);
    assert_eq!(list.deployments[0].id, bad.id);
    let latest =
        w.s.get_pages_deployment(req(
            &w.tok("reader"),
            GetPagesDeploymentRequest {
                repo: rref("acme/app"),
                pages_deployment_id: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(latest.id, bad.id);
    assert_eq!(
        reason(
            w.s.cancel_pages_deployment(req(
                &w.tok("writer"),
                CancelPagesDeploymentRequest {
                    repo: rref("acme/app"),
                    pages_deployment_id: d1.id.clone()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Permissions: readers cannot trigger builds.
    assert_eq!(
        reason(
            w.s.request_pages_build(req(
                &w.tok("reader"),
                RequestPagesBuildRequest {
                    repo: rref("acme/app")
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    // Events.
    w.flush().await;
    let kinds = w.event_kinds().await;
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "pages.deployment_succeeded")
            .count(),
        2
    );
    assert!(kinds.contains(&"pages.deployment_failed".to_string()));
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "page_build.completed")
            .count(),
        3
    );
}

#[tokio::test]
async fn source_folder_and_manifest_dedupe() {
    let w = world().await;
    create_site(&w, PagesBuildType::Ref, "/docs", PagesAccess::Public).await;
    seed(
        &w,
        &[
            ("index.html", "root"),
            ("docs/index.html", "docs home"),
            ("docs/a/b.html", "deep"),
        ],
        "c1",
    )
    .await;
    let d = build(&w).await;
    assert_eq!(
        d.state,
        PagesDeploymentState::Succeeded as i32,
        "{}",
        d.error_message
    );
    assert_eq!(
        body_str(&get(&w, HOST, "/app/", None, true).await),
        "docs home"
    );
    assert_eq!(
        body_str(&get(&w, HOST, "/app/a/b", None, true).await),
        "deep"
    );
    // The manifest is content addressed: the same files give the same manifest id.
    let org = w.env.forge.pages.site(&Cx::system(), &w.app).await.unwrap();
    assert_eq!(org.source_path, "/docs");
    let before = w.blobs.len();
    seed(&w, &[("README.md", "outside the folder")], "c2").await;
    let d2 = build(&w).await;
    assert_ne!(d2.id, d.id);
    let m1 = w
        .env
        .forge
        .pages
        .manifest(&Cx::system(), &w.app.id, &manifest_of(&w, &d.id).await)
        .await
        .unwrap();
    let m2 = w
        .env
        .forge
        .pages
        .manifest(&Cx::system(), &w.app.id, &manifest_of(&w, &d2.id).await)
        .await
        .unwrap();
    assert_eq!(m1.id, m2.id);
    assert_eq!(m1.entries.len(), 2);
    assert_eq!(w.blobs.len(), before);
    // Deleting the site removes every blob.
    w.s.delete_pages_site(req(
        &w.tok("owner"),
        DeletePagesSiteRequest {
            repo: rref("acme/app"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(w.blobs.len(), 0, "{:?}", w.blobs.keys());
}

async fn manifest_of(w: &World, dep: &str) -> String {
    let d = w
        .env
        .forge
        .pages
        .deployment(&Cx::system(), &w.app, dep)
        .await
        .unwrap();
    d.manifest_id
}

#[tokio::test]
async fn workflow_artifact_deployments() {
    let w = world().await;
    create_site(&w, PagesBuildType::Workflow, "/", PagesAccess::Public).await;
    // A workflow site is not built from a ref; a ref site does not take artifacts.
    assert_eq!(
        reason(
            w.s.request_pages_build(req(
                &w.tok("writer"),
                RequestPagesBuildRequest {
                    repo: rref("acme/app")
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    let art = |files: &[(&str, &[u8])]| Artifact {
        data: Bytes::from(gzip(&tar(files))),
        run_id: "run-3".into(),
        changeset: None,
    };
    w.artifacts.put(
        &w.app.id,
        "a1",
        art(&[("index.html", b"<h1>artifact</h1>"), ("app.js", b"1")]),
    );
    let job = w.job_token(&w.app);
    let dep = |tok: &str, id: &str, preview: bool| {
        let t = tok.to_string();
        let s = w.s.clone();
        let id = id.to_string();
        async move {
            s.create_pages_deployment(req(
                &t,
                CreatePagesDeploymentRequest {
                    repo: rref("acme/app"),
                    artifact_id: id,
                    changeset: None,
                    environment: String::new(),
                    preview,
                },
            ))
            .await
        }
    };
    let d = dep(&job, "a1", false).await.unwrap().into_inner();
    assert_eq!(d.state, PagesDeploymentState::Succeeded as i32);
    assert_eq!(d.workflow_run_id, "run-3");
    assert_eq!(d.artifact_id, "a1");
    assert_eq!(
        body_str(&get(&w, HOST, "/app/", None, true).await),
        "<h1>artifact</h1>"
    );
    // The job token is the `pages: write` credential; a reader's token is not.
    assert_eq!(
        reason(dep(&w.tok("reader"), "a1", false).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(reason(dep(&job, "nope", false).await), "NOT_FOUND");
    // Zip artifacts work too, one entry deflated.
    w.artifacts.put(
        &w.app.id,
        "z1",
        Artifact {
            data: Bytes::from(zip(
                &[("index.html", b"from zip zip zip zip zip"), ("b.txt", b"b")],
                "index.html",
            )),
            run_id: "run-4".into(),
            changeset: None,
        },
    );
    dep(&job, "z1", false).await.unwrap();
    assert_eq!(
        body_str(&get(&w, HOST, "/app/", None, true).await),
        "from zip zip zip zip zip"
    );
    // Size limit: more than the configured 8 files is TOO_LARGE.
    let many: Vec<(String, Vec<u8>)> = (0..9).map(|i| (format!("f{i}.html"), vec![b'x'])).collect();
    let refs: Vec<(&str, &[u8])> = many
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    w.artifacts.put(&w.app.id, "big", art(&refs));
    assert_eq!(reason(dep(&job, "big", false).await), "TOO_LARGE");
    // A preview deployment goes to its own URL and leaves the live site alone.
    w.artifacts
        .put(&w.app.id, "p1", art(&[("index.html", b"preview build")]));
    let p = dep(&job, "p1", true).await.unwrap().into_inner();
    assert!(
        p.preview_url.starts_with("https://app-preview-"),
        "{}",
        p.preview_url
    );
    let host = p
        .preview_url
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string();
    assert_eq!(
        body_str(&get(&w, &host, "/", None, true).await),
        "preview build"
    );
    assert_eq!(
        body_str(&get(&w, HOST, "/app/", None, true).await),
        "from zip zip zip zip zip"
    );
    // A protected environment needs approval.
    w.gate
        .protected
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(reason(dep(&job, "a1", false).await), "APPROVAL_REQUIRED");
}

#[tokio::test]
async fn private_sites_follow_repo_read_access() {
    let w = world().await;
    create_site(&w, PagesBuildType::Ref, "/", PagesAccess::Private).await;
    seed(&w, &[("index.html", "secret")], "c").await;
    build(&w).await;
    // Anonymous: asked to sign in. Reader: served. Outsider: 404.
    assert_eq!(get(&w, HOST, "/app/", None, true).await.status(), 401);
    assert_eq!(
        body_str(&get(&w, HOST, "/app/", Some(&w.caller("reader")), true).await),
        "secret"
    );
    assert_eq!(
        get(&w, HOST, "/app/", Some(&w.caller("outsider")), true)
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn previews_per_change_request() {
    let w = world().await;
    create_site(&w, PagesBuildType::Ref, "/", PagesAccess::Public).await;
    let main = seed(&w, &[("index.html", "main")], "main").await;
    build(&w).await;
    let cx = Cx::system();
    let pages = &w.env.forge.pages;
    let head1 = seed(&w, &[("index.html", "pr one")], "cr1").await;
    w.vcs.set_ref(&w.vr(&w.app), "main", &main).unwrap();
    let d = pages
        .deploy_change_preview(&cx, &w.app, 7, &head1)
        .await
        .unwrap();
    assert_eq!(d.preview_url, "https://app-pr-7.acme.pages.test/");
    assert_eq!(
        body_str(&get(&w, "app-pr-7.acme.pages.test", "/", None, true).await),
        "pr one"
    );
    // The live site is untouched.
    assert_eq!(body_str(&get(&w, HOST, "/app/", None, true).await), "main");
    // A new revision replaces the preview at the same host.
    let head2 = seed(&w, &[("index.html", "pr two")], "cr1b").await;
    w.vcs.set_ref(&w.vr(&w.app), "main", &main).unwrap();
    pages
        .deploy_change_preview(&cx, &w.app, 7, &head2)
        .await
        .unwrap();
    assert_eq!(
        body_str(&get(&w, "app-pr-7.acme.pages.test", "/", None, true).await),
        "pr two"
    );
    // Removal on close.
    assert!(pages.remove_change_preview(&cx, &w.app, 7).await.unwrap());
    assert_eq!(
        get(&w, "app-pr-7.acme.pages.test", "/", None, true)
            .await
            .status(),
        404
    );
    assert!(!pages.remove_change_preview(&cx, &w.app, 7).await.unwrap());
    // Expiry: previews vanish after their lifetime and maintenance cleans them up.
    pages
        .deploy_change_preview(&cx, &w.app, 8, &head1)
        .await
        .unwrap();
    assert_eq!(
        get(&w, "app-pr-8.acme.pages.test", "/", None, true)
            .await
            .status(),
        200
    );
    w.clock.advance(15 * 86_400_000);
    assert_eq!(
        get(&w, "app-pr-8.acme.pages.test", "/", None, true)
            .await
            .status(),
        404
    );
    w.env.forge.maintain().await.unwrap();
    let list =
        w.s.list_pages_deployments(req(
            &w.tok("owner"),
            ListPagesDeploymentsRequest {
                repo: rref("acme/app"),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        list.deployments.iter().all(|d| d.preview_url.is_empty()),
        "{:?}",
        list.deployments
            .iter()
            .map(|d| &d.preview_url)
            .collect::<Vec<_>>()
    );
    // Previews can be turned off per site.
    pages
        .set_preview_settings(&cx, &w.app, false, 0)
        .await
        .unwrap();
    assert!(
        pages
            .deploy_change_preview(&cx, &w.app, 9, &head1)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn events_drive_rebuilds_and_previews() {
    let w = world().await;
    create_site(&w, PagesBuildType::Ref, "/", PagesAccess::Public).await;
    let c1 = seed(&w, &[("index.html", "one")], "c1").await;
    let cx = Cx::system();
    // A push to the source ref rebuilds the site.
    let ev = Event {
        kind: "ref.updated".into(),
        repo_id: w.app.id.clone(),
        org_id: w.app.org_id.clone(),
        body: Some(event::Body::RefUpdated(Ref {
            name: "main".into(),
            target: Some(c1.clone()),
            version: 1,
        })),
        ..Default::default()
    };
    w.env.forge.events.append(&cx, ev, "t1").await.unwrap();
    wait_for(&w, HOST, "/app/", "one").await;
    // A change request opened against it gets a preview; closing it removes the preview.
    let head = seed(&w, &[("index.html", "cr")], "cr").await;
    w.vcs.set_ref(&w.vr(&w.app), "main", &c1).unwrap();
    let cr = ChangeRequest {
        id: "cr1".into(),
        repo_id: w.app.id.clone(),
        target_ref: "main".into(),
        number: 5,
        revisions: vec![ChangeRevision {
            n: 1,
            head: Some(head),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ev = |kind: &str| Event {
        kind: kind.into(),
        repo_id: w.app.id.clone(),
        org_id: w.app.org_id.clone(),
        body: Some(event::Body::Change(cr.clone())),
        ..Default::default()
    };
    w.env
        .forge
        .events
        .append(&cx, ev("cr.opened"), "t2")
        .await
        .unwrap();
    wait_for(&w, "app-pr-5.acme.pages.test", "/", "cr").await;
    w.env
        .forge
        .events
        .append(&cx, ev("cr.closed"), "t3")
        .await
        .unwrap();
    for _ in 0..100 {
        if get(&w, "app-pr-5.acme.pages.test", "/", None, true)
            .await
            .status()
            == 404
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the preview was not removed");
}

async fn wait_for(w: &World, host: &str, path: &str, want: &str) {
    for _ in 0..200 {
        let r = get(w, host, path, None, true).await;
        if r.status() == 200 && body_str(&r) == want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("{host}{path} never served {want}");
}
