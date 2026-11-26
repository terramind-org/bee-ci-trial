//! `ReleaseService`: tags, drafts, prereleases, latest, immutable releases, assets, source
//! archives and generated notes from landed change requests (AS-COLLAB-01).

use crate::community_support as support;

use bee_auth::Scope;
use bee_forge::community::ports::{ContentVcs, LandedChange};
use bee_proto::v1::discussion_service_server::DiscussionService;
use bee_proto::v1::release_service_server::ReleaseService;
use bee_proto::v1::*;
use support::*;

fn landed(number: u64, title: &str, who: &str, labels: &[&str], at: u64) -> LandedChange {
    LandedChange {
        id: format!("cr-{number}"),
        number,
        title: title.into(),
        author: human(who),
        labels: labels.iter().map(|s| s.to_string()).collect(),
        landed_at_ms: at,
    }
}

async fn create(
    w: &World,
    who: &str,
    tag: &str,
    f: impl FnOnce(&mut CreateReleaseRequest),
) -> Result<Release, String> {
    let mut r = CreateReleaseRequest {
        repo: rref("acme/app"),
        tag_name: tag.into(),
        target: Some(Revision {
            expr: "main".into(),
        }),
        ..Default::default()
    };
    f(&mut r);
    w.s.create_release(req(&w.tok(who), r))
        .await
        .map(|x| x.into_inner())
        .map_err(|st| reason::<()>(Err(st)))
}

/// A repo with one commit; returns its changeset.
async fn seed(w: &World, files: &[(&str, &str)], who: &str, msg: &str, t: u64) -> ObjectId {
    w.vcs
        .seed(&w.vr(&w.app), files, &human(who), msg, t)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_collab_01_cut_a_release_with_generated_notes_from_landed_change_requests() {
    let w = world().await;
    let agent = w.env.agent_token("writer", Scope::new());
    let vr = w.vr(&w.app);
    // History: CR #1 landed before v1.0.0; #2, #3 and #4 after it.
    let c1 = seed(&w, &[("README.md", "# app\n")], "ann", "first", T0).await;
    w.changes
        .land(&w.app.id, &c1, landed(1, "Initial import", "ann", &[], T0));
    w.s.create_release(req(
        &agent,
        CreateReleaseRequest {
            repo: rref("acme/app"),
            tag_name: "v1.0.0".into(),
            name: "1.0.0".into(),
            target: Some(Revision {
                expr: "main".into(),
            }),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let release_yml = "changelog:\n  exclude:\n    labels: [skip-changelog]\n  categories:\n    - title: Fixes\n      labels: [bug]\n    - title: New features\n      labels: [feature]\n    - title: Everything else\n      labels: ['*']\n";
    let c2 = seed(
        &w,
        &[
            (".github/release.yml", release_yml),
            ("src/a.rs", "fn a() {}\n"),
        ],
        "bob",
        "fix a",
        T0 + 1000,
    )
    .await;
    w.changes.land(
        &w.app.id,
        &c2,
        landed(2, "Fix the overflow in a()", "bob", &["bug"], T0 + 1000),
    );
    let c3 = seed(
        &w,
        &[("src/b.rs", "fn b() {}\n")],
        "ann",
        "add b",
        T0 + 2000,
    )
    .await;
    w.changes.land(
        &w.app.id,
        &c3,
        landed(3, "Add b()", "ann", &["feature"], T0 + 2000),
    );
    let c4 = seed(
        &w,
        &[("Cargo.lock", "# lock\n")],
        "carol",
        "bump",
        T0 + 3000,
    )
    .await;
    w.changes.land(
        &w.app.id,
        &c4,
        landed(4, "Bump deps", "carol", &["skip-changelog"], T0 + 3000),
    );
    let c5 = seed(&w, &[("docs/x.md", "x\n")], "dave", "docs", T0 + 4000).await;
    w.changes.land(
        &w.app.id,
        &c5,
        landed(5, "Document x", "dave", &[], T0 + 4000),
    );

    // The agent asks what the notes would say for a tag that does not exist yet.
    let notes =
        w.s.generate_release_notes(req(
            &agent,
            GenerateReleaseNotesRequest {
                repo: rref("acme/app"),
                tag_name: "v1.1.0".into(),
                target: Some(Revision {
                    expr: "main".into(),
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(notes.previous_tag_name, "v1.0.0");
    assert_eq!(
        notes.body,
        "## Fixes\n* Fix the overflow in a() by @bob in #2\n\n## New features\n* Add b() by @ann in #3\n\n## Everything else\n* Document x by @dave in #5\n\n## New Contributors\n* @bob made their first contribution in #2\n* @dave made their first contribution in #5\n\n**Full Changelog**: https://beehive.invalid/acme/app/compare/v1.0.0...v1.1.0\n"
    );
    assert_eq!(
        notes
            .change_requests
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["cr-2", "cr-3", "cr-5"]
    );
    // The same inputs give the same notes.
    let again =
        w.s.generate_release_notes(req(
            &agent,
            GenerateReleaseNotesRequest {
                repo: rref("acme/app"),
                tag_name: "v1.1.0".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again.body, notes.body);

    // Cut the release with the generated notes plus the agent's own words.
    let rel =
        w.s.create_release(req(
            &agent,
            CreateReleaseRequest {
                repo: rref("acme/app"),
                tag_name: "v1.1.0".into(),
                name: "1.1.0".into(),
                body: "Thanks all!".into(),
                generate_notes: true,
                target: Some(Revision {
                    expr: "main".into(),
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!rel.draft && rel.latest && rel.published_at.is_some());
    assert!(rel.body.starts_with("## Fixes\n") && rel.body.ends_with("Thanks all!"));
    assert_eq!(rel.target.as_ref().unwrap(), &c5);
    assert_eq!(rel.author.as_ref().unwrap().id, "agent");
    // The tag exists at the head, and the old release is no longer latest.
    let tag =
        w.s.get_repo_tag(req(
            &agent,
            GetRepoTagRequest {
                repo: rref("acme/app"),
                name: "v1.1.0".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(tag.target.unwrap(), c5);
    assert_eq!(tag.release_id, rel.id);
    let latest =
        w.s.get_latest_release(req(
            &w.tok("reader"),
            GetLatestReleaseRequest {
                repo: rref("acme/app"),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(latest.tag_name, "v1.1.0");
    let old =
        w.s.get_release_by_tag(req(
            &w.tok("reader"),
            GetReleaseByTagRequest {
                repo: rref("acme/app"),
                tag_name: "v1.0.0".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!old.latest);
    // Ship an asset: begin, upload (out of band), finalize with its digest, download.
    let begin =
        w.s.begin_release_asset_upload(req(
            &agent,
            BeginReleaseAssetUploadRequest {
                repo: rref("acme/app"),
                release_id: rel.id.clone(),
                name: "app-linux.tar.gz".into(),
                content_type: "application/gzip".into(),
                size: 1234,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(
        begin.upload_url.contains("signature=")
            && begin
                .upload_headers
                .iter()
                .any(|h| h.key == "content-length" && h.value == "1234")
    );
    assert_eq!(
        reason(
            w.s.get_release_asset_download_url(req(
                &agent,
                GetReleaseAssetDownloadUrlRequest {
                    repo: rref("acme/app"),
                    asset_id: begin.asset_id.clone()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    let sha = vec![7u8; 32];
    let asset =
        w.s.finalize_release_asset(req(
            &agent,
            FinalizeReleaseAssetRequest {
                repo: rref("acme/app"),
                asset_id: begin.asset_id.clone(),
                sha256: sha.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (asset.state, asset.sha256.clone(), asset.size),
        (ReleaseAssetState::Uploaded as i32, sha.clone(), 1234)
    );
    for _ in 0..2 {
        let dl =
            w.s.get_release_asset_download_url(req(
                &w.tok("reader"),
                GetReleaseAssetDownloadUrlRequest {
                    repo: rref("acme/app"),
                    asset_id: asset.id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((dl.sha256.clone(), dl.size), (sha.clone(), 1234));
    }
    let listed =
        w.s.list_release_assets(req(
            &w.tok("reader"),
            ListReleaseAssetsRequest {
                repo: rref("acme/app"),
                release_id: rel.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (listed.assets.len(), listed.assets[0].download_count),
        (1, 2)
    );
    // Attestations stay pending until a signer records one.
    assert_eq!(
        reason(
            w.s.get_release_attestation(req(
                &w.tok("reader"),
                GetReleaseAttestationRequest {
                    repo: rref("acme/app"),
                    release_id: rel.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let cx = w.cx("owner");
    w.env
        .forge
        .community
        .releases
        .record_attestation(&cx, &w.app, &rel.id, "{\"bundle\":true}")
        .await
        .unwrap();
    let att =
        w.s.get_release_attestation(req(
            &w.tok("reader"),
            GetReleaseAttestationRequest {
                repo: rref("acme/app"),
                release_id: rel.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (att.subjects.len(), att.subjects[0].sha256.clone()),
        (1, sha)
    );
    // Source archives are requested once per tree, format and prefix.
    let archive = |fmt: SourceArchiveFormat| {
        w.s.get_source_archive_url(req(
            &w.tok("reader"),
            GetSourceArchiveUrlRequest {
                repo: rref("acme/app"),
                at: Some(Revision {
                    expr: "v1.1.0".into(),
                }),
                format: fmt as i32,
                ..Default::default()
            },
        ))
    };
    let a1 = archive(SourceArchiveFormat::Zip)
        .await
        .unwrap()
        .into_inner();
    let a2 = archive(SourceArchiveFormat::Zip)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(a1.changeset, a2.changeset);
    assert!(a1.url.contains(".zip") && a1.url.contains("archives/"));
    archive(SourceArchiveFormat::TarGz).await.unwrap();
    let pending = w
        .env
        .forge
        .community
        .releases
        .pending_archives(&cx, 10)
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(
        pending
            .iter()
            .find(|p| p.format == SourceArchiveFormat::Zip as i32)
            .unwrap()
            .requests,
        2
    );
    w.env
        .forge
        .community
        .releases
        .mark_archive_built(&cx, &pending[0].repo_id, &pending[0].id)
        .await
        .unwrap();
    assert_eq!(
        w.env
            .forge
            .community
            .releases
            .pending_archives(&cx, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    // The release announced itself.
    w.flush().await;
    let kinds = w.event_kinds().await;
    assert_eq!(
        kinds.iter().filter(|k| *k == "release.published").count(),
        2
    );
    assert!(
        kinds.contains(&"release.asset_uploaded".to_string())
            && kinds.contains(&"tag.created".to_string())
            || kinds.contains(&"release.created".to_string())
    );
    let _ = vr;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_visibility_drafts_prereleases_and_latest() {
    let w = world().await;
    seed(&w, &[("a", "1\n")], "writer", "init", T0).await;
    // Only writers cut releases; strangers do not see the private repo at all.
    assert_eq!(
        create(&w, "reader", "v0.1.0", |_| {}).await.unwrap_err(),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        create(&w, "outsider", "v0.1.0", |_| {}).await.unwrap_err(),
        "NOT_FOUND"
    );
    assert_eq!(
        create(&w, "writer", "bad tag", |_| {}).await.unwrap_err(),
        "INVALID_ARGUMENT"
    );
    let draft = create(&w, "writer", "v0.1.0", |r| r.draft = true)
        .await
        .unwrap();
    assert!(draft.draft && draft.published_at.is_none() && !draft.latest);
    // A draft has no tag yet and is invisible to readers.
    assert_eq!(
        reason(
            w.s.get_repo_tag(req(
                &w.tok("writer"),
                GetRepoTagRequest {
                    repo: rref("acme/app"),
                    name: "v0.1.0".into()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let get = |who: &str, id: &str| {
        w.s.get_release(req(
            &w.tok(who),
            GetReleaseRequest {
                repo: rref("acme/app"),
                release_id: id.into(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(reason(get("reader", &draft.id).await), "NOT_FOUND");
    assert!(get("writer", &draft.id).await.is_ok());
    let list = |who: &str, drafts: bool, pre: bool| {
        w.s.list_releases(req(
            &w.tok(who),
            ListReleasesRequest {
                repo: rref("acme/app"),
                include_drafts: drafts,
                include_prereleases: pre,
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        list("reader", true, true)
            .await
            .unwrap()
            .into_inner()
            .releases
            .len(),
        0
    );
    assert_eq!(
        list("writer", true, true)
            .await
            .unwrap()
            .into_inner()
            .releases
            .len(),
        1
    );
    // Duplicate tags are refused.
    assert_eq!(
        create(&w, "writer", "v0.1.0", |_| {}).await.unwrap_err(),
        "ALREADY_EXISTS"
    );
    // Publishing creates the tag and makes it latest.
    let published =
        w.s.publish_release(req(
            &w.tok("writer"),
            PublishReleaseRequest {
                repo: rref("acme/app"),
                release_id: draft.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!published.draft && published.latest);
    assert!(
        w.s.get_repo_tag(req(
            &w.tok("reader"),
            GetRepoTagRequest {
                repo: rref("acme/app"),
                name: "v0.1.0".into()
            }
        ))
        .await
        .is_ok()
    );
    assert_eq!(
        reason(
            w.s.publish_release(req(
                &w.tok("writer"),
                PublishReleaseRequest {
                    repo: rref("acme/app"),
                    release_id: draft.id.clone(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // A prerelease is never latest; a later release is.
    w.clock.advance(1000);
    let pre = create(&w, "writer", "v0.2.0-rc1", |r| r.prerelease = true)
        .await
        .unwrap();
    assert!(!pre.latest && pre.prerelease);
    assert_eq!(
        w.s.get_latest_release(req(
            &w.tok("reader"),
            GetLatestReleaseRequest {
                repo: rref("acme/app"),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .tag_name,
        "v0.1.0"
    );
    assert_eq!(
        list("reader", false, false)
            .await
            .unwrap()
            .into_inner()
            .releases
            .len(),
        1
    );
    assert_eq!(
        list("reader", false, true)
            .await
            .unwrap()
            .into_inner()
            .releases
            .len(),
        2
    );
    w.clock.advance(1000);
    let r2 = create(&w, "writer", "v0.2.0", |_| {}).await.unwrap();
    assert!(r2.latest);
    // make_latest = FALSE keeps a backport from taking the crown; TRUE pins an older one.
    w.clock.advance(1000);
    let backport = create(&w, "writer", "v0.1.1", |r| {
        r.make_latest = ReleaseMakeLatest::False as i32
    })
    .await
    .unwrap();
    assert!(!backport.latest);
    assert_eq!(
        w.s.get_latest_release(req(
            &w.tok("reader"),
            GetLatestReleaseRequest {
                repo: rref("acme/app"),
                ..Default::default()
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .tag_name,
        "v0.2.0"
    );
    w.clock.advance(1000);
    let pinned =
        w.s.update_release(req(
            &w.tok("writer"),
            UpdateReleaseRequest {
                repo: rref("acme/app"),
                release_id: published.id.clone(),
                make_latest: Some(ReleaseMakeLatest::True as i32),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(pinned.latest);
    // Updates are optimistic; published releases keep their tag.
    let upd = |v: u64, f: &dyn Fn(&mut UpdateReleaseRequest)| {
        let mut r = UpdateReleaseRequest {
            repo: rref("acme/app"),
            release_id: r2.id.clone(),
            expected_version: v,
            ..Default::default()
        };
        f(&mut r);
        w.s.update_release(req(&w.tok("writer"), r))
    };
    assert_eq!(
        reason(upd(r2.version + 9, &|r| r.name = Some("x".into())).await),
        "VERSION_MISMATCH"
    );
    assert_eq!(
        reason(upd(0, &|r| r.tag_name = Some("v9".into())).await),
        "INVALID_STATE"
    );
    let renamed = upd(r2.version, &|r| r.name = Some("Two".into()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(renamed.name, "Two");
    // Deleting a release keeps its tag unless asked.
    w.s.delete_release(req(
        &w.tok("writer"),
        DeleteReleaseRequest {
            repo: rref("acme/app"),
            release_id: backport.id.clone(),
            delete_tag: false,
        },
    ))
    .await
    .unwrap();
    assert!(
        w.s.get_repo_tag(req(
            &w.tok("reader"),
            GetRepoTagRequest {
                repo: rref("acme/app"),
                name: "v0.1.1".into()
            }
        ))
        .await
        .is_ok()
    );
    let by_tag =
        w.s.get_repo_tag(req(
            &w.tok("reader"),
            GetRepoTagRequest {
                repo: rref("acme/app"),
                name: "v0.2.0".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(by_tag.release_id, r2.id);
    // Pagination.
    let p1 =
        w.s.list_releases(req(
            &w.tok("reader"),
            ListReleasesRequest {
                repo: rref("acme/app"),
                include_prereleases: true,
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
    assert_eq!(p1.releases.len(), 2);
    let p2 =
        w.s.list_releases(req(
            &w.tok("reader"),
            ListReleasesRequest {
                repo: rref("acme/app"),
                include_prereleases: true,
                page: Some(PageRequest {
                    page_size: 2,
                    page_token: p1.page.unwrap().next_page_token,
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(p2.releases.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn immutable_releases_freeze_their_tag_and_assets() {
    let w = world().await;
    seed(&w, &[("a", "1\n")], "writer", "init", T0).await;
    assert_eq!(
        create(&w, "writer", "v1", |r| r.immutable = true)
            .await
            .unwrap_err(),
        "INVALID_ARGUMENT"
    );
    let draft = create(&w, "writer", "v1", |r| {
        r.draft = true;
        r.immutable = true;
    })
    .await
    .unwrap();
    let up = |release: &str, name: &str| {
        w.s.begin_release_asset_upload(req(
            &w.tok("writer"),
            BeginReleaseAssetUploadRequest {
                repo: rref("acme/app"),
                release_id: release.into(),
                name: name.into(),
                size: 10,
                ..Default::default()
            },
        ))
    };
    let begin = up(&draft.id, "a.zip").await.unwrap().into_inner();
    assert_eq!(reason(up(&draft.id, "a.zip").await), "ALREADY_EXISTS");
    assert_eq!(
        reason(
            w.s.begin_release_asset_upload(req(
                &w.tok("writer"),
                BeginReleaseAssetUploadRequest {
                    repo: rref("acme/app"),
                    release_id: draft.id.clone(),
                    name: "z".into(),
                    size: 0,
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let fin = |id: &str| {
        w.s.finalize_release_asset(req(
            &w.tok("writer"),
            FinalizeReleaseAssetRequest {
                repo: rref("acme/app"),
                asset_id: id.into(),
                sha256: vec![1; 32],
            },
        ))
    };
    assert_eq!(
        reason(
            w.s.finalize_release_asset(req(
                &w.tok("writer"),
                FinalizeReleaseAssetRequest {
                    repo: rref("acme/app"),
                    asset_id: begin.asset_id.clone(),
                    sha256: vec![1; 3]
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    fin(&begin.asset_id).await.unwrap();
    // Assets can be renamed while drafting.
    let renamed =
        w.s.update_release_asset(req(
            &w.tok("writer"),
            UpdateReleaseAssetRequest {
                repo: rref("acme/app"),
                asset_id: begin.asset_id.clone(),
                name: Some("app.zip".into()),
                label: Some("Source".into()),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (renamed.name.as_str(), renamed.label.as_str()),
        ("app.zip", "Source")
    );
    let published =
        w.s.publish_release(req(
            &w.tok("writer"),
            PublishReleaseRequest {
                repo: rref("acme/app"),
                release_id: draft.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(published.immutable);
    // Frozen: assets, tag, target, and the tag itself.
    assert_eq!(reason(up(&draft.id, "b.zip").await), "INVALID_STATE");
    assert_eq!(
        reason(
            w.s.delete_release_asset(req(
                &w.tok("writer"),
                DeleteReleaseAssetRequest {
                    repo: rref("acme/app"),
                    asset_id: begin.asset_id.clone()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    assert_eq!(
        reason(
            w.s.update_release_asset(req(
                &w.tok("writer"),
                UpdateReleaseAssetRequest {
                    repo: rref("acme/app"),
                    asset_id: begin.asset_id.clone(),
                    name: Some("x".into()),
                    label: None
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    assert_eq!(
        reason(
            w.s.update_release(req(
                &w.tok("writer"),
                UpdateReleaseRequest {
                    repo: rref("acme/app"),
                    release_id: draft.id.clone(),
                    tag_name: Some("v2".into()),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    assert_eq!(
        reason(
            w.s.update_release(req(
                &w.tok("writer"),
                UpdateReleaseRequest {
                    repo: rref("acme/app"),
                    release_id: draft.id.clone(),
                    draft: Some(true),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Titles and notes stay editable.
    assert!(
        w.s.update_release(req(
            &w.tok("writer"),
            UpdateReleaseRequest {
                repo: rref("acme/app"),
                release_id: draft.id.clone(),
                body: Some("notes".into()),
                ..Default::default()
            }
        ))
        .await
        .is_ok()
    );
    assert_eq!(
        reason(
            w.s.delete_repo_tag(req(
                &w.tok("writer"),
                DeleteRepoTagRequest {
                    repo: rref("acme/app"),
                    name: "v1".into(),
                    expected_version: 0
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Deleting the tag with the release needs `manage_refs` on top of write access.
    assert_eq!(
        reason(
            w.s.delete_release(req(
                &w.tok("writer"),
                DeleteReleaseRequest {
                    repo: rref("acme/app"),
                    release_id: draft.id.clone(),
                    delete_tag: true
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(
            w.s.delete_release(req(
                &w.tok("admin"),
                DeleteReleaseRequest {
                    repo: rref("acme/app"),
                    release_id: draft.id.clone(),
                    delete_tag: true
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );
    // Deleting the release leaves a tombstone: the tag name can never be reused.
    w.s.delete_release(req(
        &w.tok("writer"),
        DeleteReleaseRequest {
            repo: rref("acme/app"),
            release_id: draft.id.clone(),
            delete_tag: false,
        },
    ))
    .await
    .unwrap();
    let again = create(&w, "writer", "v1", |_| {}).await;
    assert_eq!(again.unwrap_err(), "ALREADY_EXISTS");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tags_lightweight_annotated_prefixed_and_protected_by_use() {
    let w = world().await;
    let c1 = seed(&w, &[("a", "1\n")], "writer", "init", T0).await;
    let tag = |who: &str, name: &str, message: &str, sig: &[u8]| {
        w.s.create_repo_tag(req(
            &w.tok(who),
            CreateRepoTagRequest {
                repo: rref("acme/app"),
                name: name.into(),
                target: Some(Revision {
                    expr: "main".into(),
                }),
                message: message.into(),
                signature: sig.to_vec(),
            },
        ))
    };
    assert_eq!(
        reason(tag("reader", "t1", "", b"").await),
        "PERMISSION_DENIED"
    );
    let light = tag("writer", "t1", "", b"").await.unwrap().into_inner();
    assert!(!light.annotated && light.target.as_ref() == Some(&c1));
    let ann = tag("writer", "rel/t2", "second tag", b"")
        .await
        .unwrap()
        .into_inner();
    assert!(ann.annotated);
    let a = ann.annotation.unwrap();
    assert_eq!(
        (
            a.message.as_str(),
            a.tagger.unwrap().id.as_str(),
            a.verification_reason.as_str()
        ),
        ("second tag", "writer", "unsigned")
    );
    assert_eq!(reason(tag("writer", "t1", "", b"").await), "ALREADY_EXISTS");
    assert_eq!(
        reason(tag("writer", "t3", "", b"-----BEGIN PGP SIGNATURE-----").await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(tag("writer", "bad..name", "", b"").await),
        "INVALID_ARGUMENT"
    );
    let signed = tag("writer", "t4", "signed", b"-----BEGIN PGP SIGNATURE-----")
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        signed.annotation.unwrap().verification_reason,
        "unknown_key"
    );
    let list = |prefix: &str| {
        w.s.list_repo_tags(req(
            &w.tok("reader"),
            ListRepoTagsRequest {
                repo: rref("acme/app"),
                prefix: prefix.into(),
                ..Default::default()
            },
        ))
    };
    assert_eq!(
        list("")
            .await
            .unwrap()
            .into_inner()
            .tags
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        vec!["rel/t2", "t1", "t4"]
    );
    assert_eq!(list("rel/").await.unwrap().into_inner().tags.len(), 1);
    // Optimistic delete.
    let del = |name: &str, v: u64| {
        w.s.delete_repo_tag(req(
            &w.tok("writer"),
            DeleteRepoTagRequest {
                repo: rref("acme/app"),
                name: name.into(),
                expected_version: v,
            },
        ))
    };
    assert_eq!(reason(del("t1", 99).await), "VERSION_MISMATCH");
    del("t1", light.ref_version).await.unwrap();
    assert_eq!(reason(del("t1", 0).await), "NOT_FOUND");
    // A tag used by a published release stays until the release is deleted.
    create(&w, "writer", "t4", |_| {}).await.unwrap();
    assert_eq!(reason(del("t4", 0).await), "INVALID_STATE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn releases_open_and_link_discussions() {
    let w = world().await;
    seed(&w, &[("a", "1\n")], "writer", "init", T0).await;
    w.enable_discussions(&w.app).await;
    w.s.create_discussion_category(req(
        &w.tok("admin"),
        CreateDiscussionCategoryRequest {
            repo: rref("acme/app"),
            name: "Announcements".into(),
            format: DiscussionCategoryFormat::Announcement as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let rel = create(&w, "writer", "v1.0.0", |r| {
        r.name = "1.0".into();
        r.body = "first".into();
        r.discussion_category_id = "announcements".into();
    })
    .await
    .unwrap();
    assert!(!rel.discussion_id.is_empty());
    let d =
        w.s.get_discussion(req(
            &w.tok("reader"),
            GetDiscussionRequest {
                id: rel.discussion_id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((d.title.as_str(), d.body.as_str()), ("1.0", "first"));
    // A draft remembers the category and opens the discussion when published.
    let draft = create(&w, "writer", "v1.1.0", |r| {
        r.draft = true;
        r.discussion_category_id = "announcements".into();
    })
    .await
    .unwrap();
    assert!(draft.discussion_id.is_empty());
    let published =
        w.s.publish_release(req(
            &w.tok("writer"),
            PublishReleaseRequest {
                repo: rref("acme/app"),
                release_id: draft.id.clone(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(!published.discussion_id.is_empty());
    // Link later; a second link is refused.
    let plain = create(&w, "writer", "v1.2.0", |_| {}).await.unwrap();
    let link = |id: &str| {
        w.s.link_release_discussion(req(
            &w.tok("writer"),
            LinkReleaseDiscussionRequest {
                repo: rref("acme/app"),
                release_id: id.into(),
                discussion_category_id: "announcements".into(),
            },
        ))
    };
    assert!(
        !link(&plain.id)
            .await
            .unwrap()
            .into_inner()
            .discussion_id
            .is_empty()
    );
    assert_eq!(reason(link(&plain.id).await), "ALREADY_EXISTS");
    let other = create(&w, "writer", "v1.3.0", |_| {}).await.unwrap();
    assert_eq!(
        reason(
            w.s.link_release_discussion(req(
                &w.tok("writer"),
                LinkReleaseDiscussionRequest {
                    repo: rref("acme/app"),
                    release_id: other.id.clone(),
                    discussion_category_id: "nope".into()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generated_notes_edge_cases() {
    let w = world().await;
    let c1 = seed(&w, &[("a", "1\n")], "ann", "init", T0).await;
    w.changes
        .land(&w.app.id, &c1, landed(1, "Start", "ann", &[], T0));
    let gen_notes = |tag: &str, prev: &str, cfg: &str| {
        w.s.generate_release_notes(req(
            &w.tok("writer"),
            GenerateReleaseNotesRequest {
                repo: rref("acme/app"),
                tag_name: tag.into(),
                previous_tag_name: prev.into(),
                config_path: cfg.as_bytes().to_vec(),
                ..Default::default()
            },
        ))
    };
    // First release: everything so far, no contributor section, no compare link.
    let first = gen_notes("v0.1.0", "", "").await.unwrap().into_inner();
    assert_eq!(first.body, "## What's Changed\n* Start by @ann in #1\n");
    assert_eq!(first.previous_tag_name, "");
    assert_eq!(
        reason(gen_notes("v0.2.0", "nope", "").await),
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        reason(gen_notes("bad tag", "", "").await),
        "INVALID_ARGUMENT"
    );
    // A broken config is the caller's error; a missing one is the default.
    let c2 = seed(
        &w,
        &[("custom.yml", "changelog: [unclosed\n")],
        "ann",
        "cfg",
        T0 + 1,
    )
    .await;
    w.changes
        .land(&w.app.id, &c2, landed(2, "More", "ann", &[], T0 + 1));
    assert_eq!(
        reason(gen_notes("v0.2.0", "", "custom.yml").await),
        "INVALID_ARGUMENT"
    );
    assert!(gen_notes("v0.2.0", "", "does-not-exist.yml").await.is_ok());
    // Readers cannot ask.
    assert_eq!(
        reason(
            w.s.generate_release_notes(req(
                &w.tok("reader"),
                GenerateReleaseNotesRequest {
                    repo: rref("acme/app"),
                    tag_name: "v0.3.0".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    // Nothing new since the previous tag: just the compare link.
    w.s.create_release(req(
        &w.tok("writer"),
        CreateReleaseRequest {
            repo: rref("acme/app"),
            tag_name: "v0.2.0".into(),
            target: Some(Revision {
                expr: "main".into(),
            }),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let empty = gen_notes("v0.3.0", "", "").await.unwrap().into_inner();
    assert_eq!(
        empty.body,
        "**Full Changelog**: https://beehive.invalid/acme/app/compare/v0.2.0...v0.3.0\n"
    );
    assert!(empty.change_requests.is_empty());
    let _ = w.vcs.head(&w.vr(&w.app)).await;
}
