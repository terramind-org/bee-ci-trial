//! Evidence of change requests (SPEC §11.9) through `BriefService` and the signed
//! `/_evidence/...` URLs, on the memory and SQLite stores: Begin → PUT → Finalize with the
//! server's digest, inline sniffing, CI evidence (reporter and revision rules, who may delete
//! it), evidence from workflow artifacts, staleness after a new revision, listing order and
//! heroes, updates, the per-change limits, URLs bound to the principal chain, and the upkeep of
//! abandoned uploads.

use crate::briefs_on_both;
use crate::briefs_support::{World, fail, png, rref, sha};
use crate::common;
use bee_forge::briefs::PENDING_TTL_MS;
use bee_forge::checks::Report;
use bee_proto::v1::brief_service_server::BriefService;
use bee_proto::v1::*;

fn main_png(alt: &str) -> InlineEvidenceAsset {
    InlineEvidenceAsset {
        role: EvidenceAssetRole::Main as i32,
        content_type: "image/png".into(),
        data: png(1440, 900),
        alt: alt.into(),
        ..Default::default()
    }
}

async fn add(
    w: &World,
    who: &str,
    req: AddEvidenceRequest,
) -> std::result::Result<tonic::Response<Evidence>, tonic::Status> {
    w.b.add_evidence(common::req(&w.tok(who), req)).await
}

fn inline(
    w: &World,
    cr: &ChangeRequest,
    spec: EvidenceSpec,
    assets: Vec<InlineEvidenceAsset>,
) -> AddEvidenceRequest {
    AddEvidenceRequest {
        repo: rref(&w.app),
        change_id: cr.id.clone(),
        spec: Some(spec),
        assets,
        ..Default::default()
    }
}

fn spec(kind: EvidenceKind, title: &str) -> EvidenceSpec {
    EvidenceSpec {
        kind: kind as i32,
        title: title.into(),
        ..Default::default()
    }
}

async fn get(w: &World, who: &str, id: &str) -> Evidence {
    w.b.get_evidence(common::req(
        &w.tok(who),
        GetEvidenceRequest {
            repo: rref(&w.app),
            id: id.into(),
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn list(w: &World, ids: &[&str], hero_only: bool) -> Vec<Evidence> {
    w.b.list_evidence(common::req(
        &w.tok("reader"),
        ListEvidenceRequest {
            repo: rref(&w.app),
            change_ids: ids.iter().map(|s| s.to_string()).collect(),
            hero_only,
            page: None,
        },
    ))
    .await
    .unwrap()
    .into_inner()
    .evidence
}

async fn begin_put_finalize_and_integrity(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("web/checkout.tsx", "export {}\n")])
        .await;
    let before = png(1440, 900);
    let after = png(1440, 901);
    let begun =
        w.b.begin_evidence_upload(common::req(
            &w.tok("agent"),
            BeginEvidenceUploadRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
                spec: Some(spec(
                    EvidenceKind::BeforeAfter,
                    "Checkout, before and after",
                )),
                revision: 0,
                check_run_id: String::new(),
                assets: vec![
                    EvidenceUploadAsset {
                        role: EvidenceAssetRole::Before as i32,
                        content_type: "image/png".into(),
                        size_bytes: before.len() as u64,
                        alt: "The old checkout".into(),
                        ..Default::default()
                    },
                    EvidenceUploadAsset {
                        role: EvidenceAssetRole::After as i32,
                        content_type: "image/png".into(),
                        size_bytes: after.len() as u64,
                        alt: "The new checkout".into(),
                        ..Default::default()
                    },
                ],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let e = begun.evidence.unwrap();
    assert_eq!(e.state, EvidenceState::Pending as i32);
    assert!(
        e.assets.iter().all(|a| a.url.is_empty()),
        "nothing to download yet"
    );
    assert_eq!(begun.uploads.len(), 2);
    let up: Vec<String> = begun
        .uploads
        .iter()
        .map(|u| World::target(&u.upload_url))
        .collect();
    // Pending items are not listed, and nothing was announced yet.
    assert!(list(&w, &[&cr.id], false).await.is_empty());
    assert!(!w.kinds(&w.app).await.iter().any(|k| k == "evidence.added"));
    // Finalizing before every asset arrived names the missing one.
    let digests = vec![
        EvidenceAssetDigest {
            role: EvidenceAssetRole::Before as i32,
            sha256: sha(&before),
        },
        EvidenceAssetDigest {
            role: EvidenceAssetRole::After as i32,
            sha256: sha(&after),
        },
    ];
    let fin = |d: Vec<EvidenceAssetDigest>| {
        w.b.finalize_evidence(common::req(
            &w.tok("agent"),
            FinalizeEvidenceRequest {
                repo: rref(&w.app),
                evidence_id: e.id.clone(),
                digests: d,
            },
        ))
    };
    let t0 = w.briefs.verify_upload(&up[0]).unwrap();
    assert_eq!((t0.evidence_id.as_str(), t0.index), (e.id.as_str(), 0));
    w.briefs.receive_upload(&t0, before.clone()).await.unwrap();
    assert_eq!(
        fail(fin(digests.clone()).await),
        "INVALID_ARGUMENT/missing_asset"
    );
    // The bytes must be what was declared: SVG uploaded for a PNG is refused.
    let t1 = w.briefs.verify_upload(&up[1]).unwrap();
    let e2 = w
        .briefs
        .receive_upload(&t1, b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec())
        .await
        .unwrap_err();
    assert_eq!(e2.subcode, "bad_media");
    w.briefs.receive_upload(&t1, after.clone()).await.unwrap();
    // A digest the server did not compute is an integrity failure.
    let mut wrong = digests.clone();
    wrong[1].sha256 = sha(b"something else");
    assert_eq!(fail(fin(wrong).await), "INTEGRITY/");
    // A tampered or swapped URL is refused.
    let tampered = up[0].replace("/0/upload", "/1/upload");
    assert_eq!(
        w.briefs.verify_upload(&tampered).unwrap_err().reason,
        ErrorReason::PermissionDenied
    );
    let ready = fin(digests.clone()).await.unwrap().into_inner();
    assert_eq!(ready.state, EvidenceState::Ready as i32);
    assert_eq!(ready.source, EvidenceSource::Uploaded as i32);
    assert_eq!(ready.revision, 1);
    assert_eq!(ready.uploader.as_ref().unwrap().id, "agent");
    assert_eq!(ready.assets[0].digest, format!("sha256:{}", sha(&before)));
    assert_eq!((ready.assets[1].width, ready.assets[1].height), (1440, 901));
    // Finalizing again, or uploading after, is the wrong state.
    assert_eq!(fail(fin(digests).await), "INVALID_STATE/");
    assert_eq!(
        w.briefs
            .receive_upload(&t0, before.clone())
            .await
            .unwrap_err()
            .reason,
        ErrorReason::InvalidState
    );
    // Downloads: the URL names the reader's chain, and the bytes come back as uploaded.
    let got = get(&w, "reader", &e.id).await;
    let url = World::target(&got.assets[1].url);
    let target = w.briefs.verify_download(&url).unwrap();
    assert_eq!(
        target.chain,
        vec![("reader".to_string(), "human".to_string())]
    );
    let d = w.briefs.download(&target).await.unwrap();
    assert_eq!(d.bytes, after);
    assert_eq!(d.content_type, "image/png");
    assert_eq!(d.disposition, None);
    // Changing the chain in the URL breaks the signature; an upload URL is no download URL.
    let other = bee_forge::media::upload::encode_chain(&[("owner".into(), "human".into())]);
    let swapped = format!(
        "{}/as/{other}?{}",
        url.split("/as/").next().unwrap(),
        url.split_once('?').unwrap().1
    );
    assert_eq!(
        w.briefs.verify_download(&swapped).unwrap_err().reason,
        ErrorReason::PermissionDenied
    );
    assert!(w.briefs.verify_download(&up[0]).is_err());
    // URLs expire.
    w.clock.advance(16 * 60 * 1000);
    assert_eq!(
        w.briefs.verify_download(&url).unwrap_err().reason,
        ErrorReason::TokenExpired
    );
    let kinds = w.kinds(&w.app).await;
    assert_eq!(kinds.iter().filter(|k| *k == "evidence.added").count(), 1);
    let audit = w.audit().await;
    assert!(audit.iter().any(|a| a == "evidence.begin_upload"));
    assert!(audit.iter().any(|a| a == "evidence.finalize"));
}
briefs_on_both!(begin_put_finalize_and_integrity);

async fn inline_sniffing_refuses_svg_and_mismatch(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("web/checkout.tsx", "export {}\n")])
        .await;
    let shot = || spec(EvidenceKind::Screenshot, "Checkout");
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>".to_vec();
    // SVG is never an accepted type, by its declared type or by its bytes.
    let mut a = main_png("The page");
    a.content_type = "image/svg+xml".into();
    a.data = svg.clone();
    assert_eq!(
        fail(add(&w, "agent", inline(&w, &cr, shot(), vec![a])).await),
        "INVALID_ARGUMENT/bad_media"
    );
    let mut a = main_png("The page");
    a.data = svg.clone();
    assert_eq!(
        fail(add(&w, "agent", inline(&w, &cr, shot(), vec![a])).await),
        "INVALID_ARGUMENT/bad_media"
    );
    // Declared and sniffed types must agree.
    let mut a = main_png("The page");
    a.content_type = "image/jpeg".into();
    assert_eq!(
        fail(add(&w, "agent", inline(&w, &cr, shot(), vec![a])).await),
        "INVALID_ARGUMENT/bad_media"
    );
    // Markup declared as a text document is refused too.
    let doc = |ct: &str, data: &[u8]| InlineEvidenceAsset {
        role: EvidenceAssetRole::Main as i32,
        content_type: ct.into(),
        data: data.to_vec(),
        ..Default::default()
    };
    for (ct, data) in [
        (
            "text/plain",
            &b"<!doctype html><script>alert(1)</script>"[..],
        ),
        ("text/html", b"<p>hi</p>"),
        ("application/javascript", b"alert(1)"),
    ] {
        assert_eq!(
            fail(
                add(
                    &w,
                    "agent",
                    inline(
                        &w,
                        &cr,
                        spec(EvidenceKind::Document, "Report"),
                        vec![doc(ct, data)]
                    )
                )
                .await
            ),
            "INVALID_ARGUMENT/bad_media",
            "{ct}"
        );
    }
    // Images need alt text; a kind takes only its roles.
    assert_eq!(
        fail(add(&w, "agent", inline(&w, &cr, shot(), vec![main_png(" ")])).await),
        "INVALID_ARGUMENT/missing_alt"
    );
    let mut a = main_png("x");
    a.role = EvidenceAssetRole::Before as i32;
    assert_eq!(
        fail(add(&w, "agent", inline(&w, &cr, shot(), vec![a])).await),
        "INVALID_ARGUMENT/bad_evidence"
    );
    // Inline requests are bounded.
    let mut big = doc("text/plain", &vec![b'a'; 9 << 20]);
    big.role = EvidenceAssetRole::Main as i32;
    assert_eq!(
        fail(
            add(
                &w,
                "agent",
                inline(&w, &cr, spec(EvidenceKind::Document, "Log"), vec![big])
            )
            .await
        ),
        "TOO_LARGE/"
    );
    // Nothing refused was stored.
    assert!(w.blobs.is_empty());
    // What is accepted is stored as sniffed and served as the kind says.
    let png_ok = add(
        &w,
        "agent",
        inline(&w, &cr, shot(), vec![main_png("The page")]),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(
        (png_ok.assets[0].width, png_ok.assets[0].height),
        (1440, 900)
    );
    let csv = add(
        &w,
        "agent",
        inline(
            &w,
            &cr,
            spec(EvidenceKind::Document, "Timings"),
            vec![doc("text/csv; charset=utf-8", b"name,ms\nload,85\n")],
        ),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(csv.assets[0].content_type, "text/csv");
    let t = w
        .briefs
        .verify_download(&World::target(&csv.assets[0].url))
        .unwrap();
    let d = w.briefs.download(&t).await.unwrap();
    assert_eq!(d.disposition, Some("attachment"));
    assert_eq!(d.bytes, b"name,ms\nload,85\n");
    // A benchmark and a preview link carry no files.
    let mut bench = spec(EvidenceKind::Benchmark, "p95 latency");
    bench.benchmark = vec![BenchmarkSeries {
        name: "p95".into(),
        unit: "ms".into(),
        baseline: 120.0,
        value: 85.0,
        lower_is_better: true,
    }];
    assert!(
        add(&w, "agent", inline(&w, &cr, bench, vec![]))
            .await
            .is_ok()
    );
    let mut link = spec(EvidenceKind::PreviewLink, "Try it");
    link.url = "http://preview.example.com".into();
    assert_eq!(
        fail(add(&w, "agent", inline(&w, &cr, link.clone(), vec![])).await),
        "INVALID_ARGUMENT/bad_url"
    );
    link.url = "https://preview.example.com/1".into();
    assert!(
        add(&w, "agent", inline(&w, &cr, link, vec![]))
            .await
            .is_ok()
    );
    // A reader may not add evidence.
    assert_eq!(
        fail(add(&w, "reader", inline(&w, &cr, shot(), vec![main_png("x")])).await),
        "PERMISSION_DENIED/"
    );
}
briefs_on_both!(inline_sniffing_refuses_svg_and_mismatch);

async fn check_run_evidence_requires_reporter_and_revision(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("src/billing.rs", "fn round() {}\n")])
        .await;
    let head = cr.revisions[0].head.clone().unwrap();
    let run = w
        .forge
        .checks
        .report(
            &w.cx("ci"),
            &w.org.id,
            &w.app.id,
            Report {
                changeset: Some(head),
                name: "test".into(),
                status: CheckStatus::Passed,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let elsewhere = w.head(&w.app, "ci", &[("other.rs", "x\n")]).await;
    let stray = w
        .forge
        .checks
        .report(
            &w.cx("ci"),
            &w.org.id,
            &w.app.id,
            Report {
                changeset: Some(elsewhere),
                name: "test".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut report = spec(EvidenceKind::TestReport, "Billing tests");
    report.test_report = Some(TestReport {
        passed: 214,
        summary: "All 214 billing tests pass".into(),
        ..Default::default()
    });
    let req = |check_run_id: &str, revision: u32| AddEvidenceRequest {
        repo: rref(&w.app),
        change_id: cr.id.clone(),
        spec: Some(report.clone()),
        check_run_id: check_run_id.into(),
        revision,
        ..Default::default()
    };
    // Only the run's reporter may attach its evidence (the CR's author may not).
    assert_eq!(
        fail(add(&w, "agent", req(&run.id, 0)).await),
        "PERMISSION_DENIED/"
    );
    // The run must have tested a revision head of this CR.
    assert_eq!(
        fail(add(&w, "ci", req(&stray.id, 0)).await),
        "INVALID_ARGUMENT/bad_evidence"
    );
    assert_eq!(
        fail(add(&w, "ci", req(&run.id, 2)).await),
        "INVALID_ARGUMENT/bad_evidence"
    );
    assert_eq!(fail(add(&w, "ci", req("01NOPE", 0)).await), "NOT_FOUND/");
    // A new revision does not move CI evidence of the tested one.
    w.revise(
        "agent",
        &w.app,
        &cr,
        &[("src/billing.rs", "fn round2() {}\n")],
    )
    .await;
    let e = add(&w, "ci", req(&run.id, 0)).await.unwrap().into_inner();
    assert_eq!(e.source, EvidenceSource::CheckRun as i32);
    assert_eq!(e.check_run_id, run.id);
    assert_eq!(e.revision, 1);
    assert_eq!(e.head, cr.revisions[0].head);
    assert!(e.stale, "captured for revision 1 of 2");
    // CI evidence is deleted only by its reporter or a maintainer, not by the CR's author.
    let del = |who: &str, id: &str| {
        w.b.delete_evidence(common::req(
            &w.tok(who),
            DeleteEvidenceRequest {
                repo: rref(&w.app),
                id: id.into(),
            },
        ))
    };
    assert_eq!(fail(del("agent", &e.id).await), "PERMISSION_DENIED/");
    assert_eq!(fail(del("writer", &e.id).await), "PERMISSION_DENIED/");
    del("owner", &e.id).await.unwrap();
    let e2 = add(&w, "ci", req(&run.id, 1)).await.unwrap().into_inner();
    del("ci", &e2.id).await.unwrap();
    // Uploaded evidence: the author may delete it.
    let mine = w
        .add_shot("agent", &w.app, &cr, "Shot")
        .await
        .unwrap()
        .into_inner();
    del("agent", &mine.id).await.unwrap();
    assert!(w.blobs.is_empty(), "deleting removes the files");
    let kinds = w.kinds(&w.app).await;
    assert_eq!(kinds.iter().filter(|k| *k == "evidence.deleted").count(), 3);
}
briefs_on_both!(check_run_evidence_requires_reporter_and_revision);

async fn evidence_goes_stale_after_new_revision(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("web/checkout.tsx", "export {}\n")])
        .await;
    let old = w
        .add_shot("agent", &w.app, &cr, "First")
        .await
        .unwrap()
        .into_inner();
    assert!(!old.stale && old.revision == 1);
    w.revise(
        "agent",
        &w.app,
        &cr,
        &[("web/checkout.tsx", "export const a = 1\n")],
    )
    .await;
    // Staleness is computed on read: the stored item did not change.
    let read = get(&w, "reader", &old.id).await;
    assert!(read.stale);
    assert_eq!(read.revision, 1);
    let fresh = w
        .add_shot("agent", &w.app, &cr, "Second")
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fresh.revision, 2);
    assert!(!fresh.stale);
    let listed = list(&w, &[&cr.id], false).await;
    let stale: Vec<(&str, bool)> = listed
        .iter()
        .map(|e| (e.spec.as_ref().unwrap().title.as_str(), e.stale))
        .collect();
    assert_eq!(stale, vec![("First", true), ("Second", false)]);
    // Evidence may be captured for an older revision on purpose.
    let back = add(
        &w,
        "agent",
        AddEvidenceRequest {
            revision: 1,
            ..inline(
                &w,
                &cr,
                spec(EvidenceKind::Screenshot, "Back"),
                vec![main_png("x")],
            )
        },
    )
    .await
    .unwrap()
    .into_inner();
    assert!(back.stale && back.revision == 1);
    assert_eq!(
        fail(
            add(
                &w,
                "agent",
                AddEvidenceRequest {
                    revision: 7,
                    ..inline(
                        &w,
                        &cr,
                        spec(EvidenceKind::Screenshot, "No"),
                        vec![main_png("x")]
                    )
                },
            )
            .await
        ),
        "INVALID_ARGUMENT/bad_evidence"
    );
}
briefs_on_both!(evidence_goes_stale_after_new_revision);

async fn listing_heroes_updates_and_limits(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("web/checkout.tsx", "export {}\n")])
        .await;
    let other = w.open_cr("writer", &w.app, &[("docs/a.md", "a\n")]).await;
    let a = w
        .add_shot("agent", &w.app, &cr, "A")
        .await
        .unwrap()
        .into_inner();
    let mut hero = spec(EvidenceKind::Screenshot, "Hero");
    hero.hero = true;
    let h = add(
        &w,
        "agent",
        inline(&w, &cr, hero.clone(), vec![main_png("x")]),
    )
    .await
    .unwrap()
    .into_inner();
    let o = w
        .add_shot("writer", &w.app, &other, "Other")
        .await
        .unwrap()
        .into_inner();
    let titles = |v: &[Evidence]| -> Vec<String> {
        v.iter()
            .map(|e| e.spec.as_ref().unwrap().title.clone())
            .collect()
    };
    assert_eq!(
        titles(&list(&w, &[&cr.id, &other.id], false).await),
        vec!["Hero", "A", "Other"]
    );
    assert_eq!(
        titles(&list(&w, &[&other.id, &cr.id, &other.id], true).await),
        vec!["Other", "Hero"]
    );
    // A new hero takes over.
    let h2 = add(&w, "agent", inline(&w, &cr, hero, vec![main_png("x")]))
        .await
        .unwrap()
        .into_inner();
    assert!(!get(&w, "reader", &h.id).await.spec.unwrap().hero);
    assert_eq!(list(&w, &[&cr.id], true).await[0].id, h2.id);
    // Updates replace the spec and alt text; the kind never changes.
    let up = |s: EvidenceSpec, alts: Vec<EvidenceAltText>| {
        w.b.update_evidence(common::req(
            &w.tok("agent"),
            UpdateEvidenceRequest {
                repo: rref(&w.app),
                id: a.id.clone(),
                spec: Some(s),
                alt_texts: alts,
            },
        ))
    };
    let mut renamed = spec(EvidenceKind::Screenshot, "Renamed");
    renamed.order = -1;
    let got = up(
        renamed,
        vec![EvidenceAltText {
            role: EvidenceAssetRole::Main as i32,
            alt: "New alt".into(),
        }],
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(got.assets[0].alt, "New alt");
    assert_eq!(
        got.assets[0].digest, a.assets[0].digest,
        "the files never change"
    );
    assert_eq!(
        fail(up(spec(EvidenceKind::Video, "V"), vec![]).await),
        "INVALID_ARGUMENT/kind_immutable"
    );
    assert_eq!(
        fail(
            up(
                spec(EvidenceKind::Screenshot, "S"),
                vec![EvidenceAltText {
                    role: EvidenceAssetRole::Main as i32,
                    alt: String::new(),
                }]
            )
            .await
        ),
        "INVALID_ARGUMENT/missing_alt"
    );
    assert_eq!(
        titles(&list(&w, &[&cr.id], false).await),
        vec!["Hero", "Renamed", "Hero"]
    );
    // At most 64 items per change request.
    let mut bench = spec(EvidenceKind::Benchmark, "Bench");
    bench.benchmark = vec![BenchmarkSeries {
        name: "p95".into(),
        unit: "ms".into(),
        baseline: 1.0,
        value: 1.0,
        lower_is_better: true,
    }];
    for _ in 0..61 {
        add(&w, "agent", inline(&w, &cr, bench.clone(), vec![]))
            .await
            .unwrap();
    }
    let e = add(&w, "agent", inline(&w, &cr, bench.clone(), vec![]))
        .await
        .expect_err("item 65");
    let d = bee_proto::errors::detail_from_status(&e).unwrap();
    assert_eq!(d.reason, "TOO_LARGE");
    // Listing names at most 100 change requests.
    let many: Vec<String> = (0..101).map(|i| format!("cr{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        fail(
            w.b.list_evidence(common::req(
                &w.tok("reader"),
                ListEvidenceRequest {
                    repo: rref(&w.app),
                    change_ids: refs.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                },
            ))
            .await
        ),
        "TOO_LARGE/"
    );
    // Paging walks the whole list.
    let mut seen = Vec::new();
    let mut token = String::new();
    loop {
        let page =
            w.b.list_evidence(common::req(
                &w.tok("reader"),
                ListEvidenceRequest {
                    repo: rref(&w.app),
                    change_ids: vec![cr.id.clone()],
                    hero_only: false,
                    page: Some(PageRequest {
                        page_size: 25,
                        page_token: token.clone(),
                    }),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        seen.extend(page.evidence.into_iter().map(|e| e.id));
        token = page.page.unwrap().next_page_token;
        if token.is_empty() {
            break;
        }
    }
    assert_eq!(seen.len(), 64);
    let _ = o;
}
briefs_on_both!(listing_heroes_updates_and_limits);

async fn artifact_evidence_is_copied_and_checked(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("web/checkout.tsx", "export {}\n")])
        .await;
    let head = cr.revisions[0].head.clone().unwrap();
    let zip = bee_forge::ci::util::zip(&[
        ("shots/after.png".into(), png(800, 600)),
        ("report.md".into(), b"# Tests\n\nAll pass\n".to_vec()),
        ("evil.html".into(), b"<script>alert(1)</script>".to_vec()),
    ]);
    let art = |id: &str, changeset: ObjectId, finalized: bool| WorkflowArtifact {
        id: id.into(),
        name: "evidence".into(),
        archive: true,
        head_changeset: Some(changeset),
        finalized,
        ..Default::default()
    };
    {
        let mut items = w.artifacts.items.lock().unwrap();
        items.push((art("art1", head.clone(), true), zip.clone()));
        items.push((art("art2", head.clone(), false), zip.clone()));
        let other = ObjectId { raw: vec![7; 33] };
        items.push((art("art3", other, true), zip));
    }
    let req = |id: &str, kind: EvidenceKind, path: &str, alt: &str| AddEvidenceRequest {
        repo: rref(&w.app),
        change_id: cr.id.clone(),
        spec: Some(spec(kind, "From CI")),
        workflow_artifact_id: id.into(),
        artifact_assets: vec![ArtifactEvidenceAsset {
            role: EvidenceAssetRole::Main as i32,
            path: path.into(),
            alt: alt.into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let shot = add(
        &w,
        "ci",
        req("art1", EvidenceKind::Screenshot, "shots/after.png", "After"),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(shot.source, EvidenceSource::WorkflowArtifact as i32);
    assert_eq!(shot.workflow_artifact_id, "art1");
    assert_eq!((shot.assets[0].width, shot.assets[0].height), (800, 600));
    let doc = add(
        &w,
        "ci",
        req("art1", EvidenceKind::Document, "report.md", ""),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(doc.assets[0].content_type, "text/markdown");
    assert_eq!(
        fail(
            add(
                &w,
                "ci",
                req("art1", EvidenceKind::Document, "evil.html", "")
            )
            .await
        ),
        "INVALID_ARGUMENT/bad_media"
    );
    assert_eq!(
        fail(
            add(
                &w,
                "ci",
                req("art1", EvidenceKind::Document, "missing.md", "")
            )
            .await
        ),
        "INVALID_ARGUMENT/bad_evidence"
    );
    assert_eq!(
        fail(
            add(
                &w,
                "ci",
                req("art2", EvidenceKind::Document, "report.md", "")
            )
            .await
        ),
        "INVALID_STATE/"
    );
    assert_eq!(
        fail(
            add(
                &w,
                "ci",
                req("art3", EvidenceKind::Document, "report.md", "")
            )
            .await
        ),
        "INVALID_ARGUMENT/bad_evidence"
    );
    // Inline files and artifact files do not mix.
    let mut both = req("art1", EvidenceKind::Screenshot, "shots/after.png", "After");
    both.assets = vec![main_png("x")];
    assert_eq!(
        fail(add(&w, "ci", both).await),
        "INVALID_ARGUMENT/bad_evidence"
    );
}
briefs_on_both!(artifact_evidence_is_copied_and_checked);

async fn abandoned_uploads_are_deleted(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("web/checkout.tsx", "export {}\n")])
        .await;
    let bytes = png(10, 10);
    let begun =
        w.b.begin_evidence_upload(common::req(
            &w.tok("agent"),
            BeginEvidenceUploadRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
                spec: Some(spec(EvidenceKind::Screenshot, "Shot")),
                assets: vec![EvidenceUploadAsset {
                    role: EvidenceAssetRole::Main as i32,
                    content_type: "image/png".into(),
                    size_bytes: bytes.len() as u64,
                    alt: "Shot".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let id = begun.evidence.unwrap().id;
    let t = w
        .briefs
        .verify_upload(&World::target(&begun.uploads[0].upload_url))
        .unwrap();
    w.briefs.receive_upload(&t, bytes).await.unwrap();
    assert_eq!(w.blobs.len(), 1);
    assert_eq!(w.briefs.maintain(&w.cx("owner")).await.unwrap(), 0);
    w.clock.advance(PENDING_TTL_MS);
    assert_eq!(w.briefs.maintain(&w.cx("owner")).await.unwrap(), 1);
    assert!(w.blobs.is_empty());
    assert_eq!(
        fail(
            w.b.get_evidence(common::req(
                &w.tok("agent"),
                GetEvidenceRequest {
                    repo: rref(&w.app),
                    id,
                },
            ))
            .await
        ),
        "NOT_FOUND/"
    );
}
briefs_on_both!(abandoned_uploads_are_deleted);
