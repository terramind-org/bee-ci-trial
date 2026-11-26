//! The brief of a change request (SPEC §11.9) through `BriefService` and `ChangeService`, on
//! the memory and SQLite stores: `SetChangeBrief` (limits, server-set fields, versions, who may
//! write, `cr.brief_set`, audit), `OpenChange.brief`, and the checklist across revisions.

use crate::briefs_on_both;
use crate::briefs_support::{World, fail, png, rref};
use crate::common;
use bee_forge::changes::ChangeVcs;
use bee_forge::changes::vcs::NewCommit;
use bee_proto::v1::brief_service_server::BriefService;
use bee_proto::v1::change_service_server::ChangeService;
use bee_proto::v1::*;

fn brief() -> ChangeBrief {
    ChangeBrief {
        outcome: "Checkout shows the saved card first".into(),
        highlights: vec![BriefHighlight {
            text: "Saved cards load before the form".into(),
            paths: vec!["web/checkout.tsx".into()],
        }],
        asks: vec![BriefAsk {
            question: "Ship behind a flag?".into(),
            options: vec!["Yes".into(), "No".into()],
        }],
        risks: vec![],
        rollback: String::new(),
        try_it_url: "https://preview.example.com/cr/1".into(),
        written_for_revision: 42,
        author: Some(Principal {
            id: "spoofed".into(),
            ..Default::default()
        }),
        updated_at: None,
    }
}

async fn set(
    w: &World,
    who: &str,
    cr: &ChangeRequest,
    b: ChangeBrief,
    expected_version: u64,
) -> std::result::Result<tonic::Response<SetChangeBriefResponse>, tonic::Status> {
    w.b.set_change_brief(common::req(
        &w.tok(who),
        SetChangeBriefRequest {
            repo: rref(&w.app),
            change_id: cr.id.clone(),
            brief: Some(b),
            expected_version,
        },
    ))
    .await
}

async fn checklist(w: &World, cr: &ChangeRequest, revision: u32) -> BriefChecklist {
    w.b.get_brief_checklist(common::req(
        &w.tok("reader"),
        GetBriefChecklistRequest {
            repo: rref(&w.app),
            change_id: cr.id.clone(),
            revision,
        },
    ))
    .await
    .unwrap()
    .into_inner()
}

async fn set_brief_validates_stamps_and_versions(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("web/checkout.tsx", "export {}\n")])
        .await;
    w.tick();
    let got = set(&w, "agent", &cr, brief(), 0)
        .await
        .unwrap()
        .into_inner();
    let b = got.brief.clone().unwrap();
    // The server sets who, when and for which revision; the caller's values are ignored.
    assert_eq!(b.written_for_revision, 1);
    assert_eq!(b.author.as_ref().unwrap().id, "agent");
    assert!(b.updated_at.is_some());
    assert_eq!(b.outcome, brief().outcome);
    // The checklist comes back for the current revision: the brief is satisfied, the UI
    // change still owes visual evidence.
    let c = got.checklist.unwrap();
    assert_eq!(c.revision, 1);
    let ids: Vec<(&str, bool)> = c
        .items
        .iter()
        .map(|i| (i.id.as_str(), i.satisfied))
        .collect();
    assert_eq!(ids, vec![("brief", true), ("evidence:ui", false)]);
    assert!(!c.complete);
    // The brief is on the CR wherever it is read.
    let read =
        w.c.get_change(common::req(
            &w.tok("reader"),
            GetChangeRequest {
                repo: rref(&w.app),
                id: cr.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.brief.as_deref(), Some(&b));
    assert_eq!(read.version, got.version);
    // Optimistic versions.
    assert_eq!(
        fail(set(&w, "agent", &cr, brief(), got.version + 7).await),
        "VERSION_MISMATCH/"
    );
    let again = set(&w, "agent", &cr, ChangeBrief::default(), got.version)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again.version, got.version + 1);
    assert_eq!(again.brief.unwrap().outcome, "");
    // Limits, with their subcodes.
    let mut long = brief();
    long.outcome = "x".repeat(281);
    assert_eq!(
        fail(set(&w, "agent", &cr, long, 0).await),
        "INVALID_ARGUMENT/brief_too_long"
    );
    let mut ask = brief();
    ask.asks[0].options.truncate(1);
    assert_eq!(
        fail(set(&w, "agent", &cr, ask, 0).await),
        "INVALID_ARGUMENT/bad_brief"
    );
    let mut url = brief();
    url.try_it_url = "http://preview.example.com".into();
    assert_eq!(
        fail(set(&w, "agent", &cr, url, 0).await),
        "INVALID_ARGUMENT/bad_url"
    );
    // Who may write: the author, or repo Write; a reader may not.
    assert!(set(&w, "writer", &cr, brief(), 0).await.is_ok());
    assert_eq!(
        fail(set(&w, "reader", &cr, brief(), 0).await),
        "PERMISSION_DENIED/"
    );
    let kinds = w.kinds(&w.app).await;
    assert_eq!(kinds.iter().filter(|k| *k == "cr.brief_set").count(), 3);
    let audit = w.audit().await;
    // Three writes, and the reader's refused attempt (denials are audited too).
    assert_eq!(audit.iter().filter(|a| *a == "cr.set_brief").count(), 4);
}
briefs_on_both!(set_brief_validates_stamps_and_versions);

async fn open_change_carries_a_brief(w: World) {
    let head = w
        .head(&w.app, "agent", &[("src/billing.rs", "fn round() {}\n")])
        .await;
    let open = |b: ChangeBrief| OpenChangeRequest {
        repo: rref(&w.app),
        head: Some(head.clone()),
        title: "Round invoices".into(),
        brief: Some(b),
        ..Default::default()
    };
    // A refused brief opens nothing.
    let mut bad = brief();
    bad.highlights = vec![bad.highlights[0].clone(); 21];
    assert_eq!(
        fail(
            w.c.open_change(common::req(&w.tok("agent"), open(bad)))
                .await
        ),
        "INVALID_ARGUMENT/bad_brief"
    );
    assert!(!w.kinds(&w.app).await.iter().any(|k| k == "cr.opened"));
    let cr =
        w.c.open_change(common::req(&w.tok("agent"), open(brief())))
            .await
            .unwrap()
            .into_inner();
    let b = cr.brief.as_deref().unwrap();
    assert_eq!(b.written_for_revision, 1);
    assert_eq!(b.author.as_ref().unwrap().id, "agent");
    let kinds = w.kinds(&w.app).await;
    let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(at("cr.opened") < at("cr.brief_set"));
    // An agent's logic change with an outcome and a highlight owes nothing more.
    let c = checklist(&w, &cr, 0).await;
    assert!(c.complete, "{c:?}");
    assert_eq!(c.items.len(), 1);
    assert_eq!(c.items[0].paths, vec!["src/billing.rs"]);
}
briefs_on_both!(open_change_carries_a_brief);

async fn checklist_follows_the_revision(w: World) {
    let cr = w
        .open_cr("agent", &w.app, &[("web/checkout.tsx", "export {}\n")])
        .await;
    // A human's docs change owes nothing.
    let docs = w
        .open_cr("writer", &w.app, &[("docs/guide.md", "# Guide\n")])
        .await;
    let c = checklist(&w, &docs, 0).await;
    assert!(c.complete && c.items.is_empty());
    let c = checklist(&w, &cr, 0).await;
    let ids: Vec<&str> = c.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, vec!["brief", "evidence:ui"]);
    assert_eq!(c.items[1].paths, vec!["web/checkout.tsx"]);
    set(&w, "agent", &cr, brief(), 0).await.unwrap();
    let shot = w
        .add_shot("agent", &w.app, &cr, "Checkout")
        .await
        .unwrap()
        .into_inner();
    let c = checklist(&w, &cr, 0).await;
    assert!(c.complete, "{c:?}");
    assert_eq!(c.items[1].evidence_ids, vec![shot.id.clone()]);
    // A new revision owes its own evidence; the old revision's checklist is unchanged.
    w.revise("agent", &w.app, &cr, &[("web/cart.css", ".cart {}\n")])
        .await;
    let c2 = checklist(&w, &cr, 0).await;
    assert_eq!(c2.revision, 2);
    let ui = c2.items.iter().find(|i| i.id == "evidence:ui").unwrap();
    assert!(!ui.satisfied && ui.evidence_ids.is_empty());
    assert_eq!(ui.paths, vec!["web/cart.css", "web/checkout.tsx"]);
    let c1 = checklist(&w, &cr, 1).await;
    assert_eq!(c1.revision, 1);
    assert!(c1.complete);
    assert_eq!(c1.items[1].paths, vec!["web/checkout.tsx"]);
    // An unknown revision is not found.
    assert_eq!(
        fail(
            w.b.get_brief_checklist(common::req(
                &w.tok("reader"),
                GetBriefChecklistRequest {
                    repo: rref(&w.app),
                    change_id: cr.id.clone(),
                    revision: 9,
                },
            ))
            .await
        ),
        "NOT_FOUND/"
    );
    // Outsiders cannot read a private repo's checklist.
    assert_eq!(
        fail(
            w.b.get_brief_checklist(common::req(
                &w.tok("outsider"),
                GetBriefChecklistRequest {
                    repo: rref(&w.app),
                    change_id: cr.id.clone(),
                    revision: 0,
                },
            ))
            .await
        ),
        "NOT_FOUND/"
    );
}
briefs_on_both!(checklist_follows_the_revision);

/// AS-CR-04: evidence captured for an earlier revision remains visible as stale, but cannot
/// satisfy the current revision's visual-evidence checklist item.
async fn stale_before_after_evidence_reopens_current_ui_checklist(w: World) {
    let first_head = w
        .head(
            &w.app,
            "agent",
            &[(
                "web/settings.tsx",
                "export const Settings = () => <h1>Account settings</h1>;\n",
            )],
        )
        .await;
    let cr =
        w.c.open_change(common::req(
            &w.tok("agent"),
            OpenChangeRequest {
                repo: rref(&w.app),
                head: Some(first_head),
                title: "Show account settings".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();

    let set = set(&w, "agent", &cr, brief(), 0)
        .await
        .unwrap()
        .into_inner();
    let satisfied = set
        .checklist
        .unwrap()
        .items
        .into_iter()
        .find(|item| item.id == "evidence:ui")
        .unwrap();
    assert!(!satisfied.satisfied);

    let evidence =
        w.b.add_evidence(common::req(
            &w.tok("agent"),
            AddEvidenceRequest {
                repo: rref(&w.app),
                change_id: cr.id.clone(),
                spec: Some(EvidenceSpec {
                    kind: EvidenceKind::BeforeAfter as i32,
                    title: "Settings before and after".into(),
                    ..Default::default()
                }),
                assets: vec![
                    InlineEvidenceAsset {
                        role: EvidenceAssetRole::Before as i32,
                        content_type: "image/png".into(),
                        data: png(32, 32),
                        alt: "Settings before the change".into(),
                        ..Default::default()
                    },
                    InlineEvidenceAsset {
                        role: EvidenceAssetRole::After as i32,
                        content_type: "image/png".into(),
                        data: png(32, 33),
                        alt: "Settings after the change".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let satisfied = checklist(&w, &cr, 0).await;
    let item = satisfied
        .items
        .iter()
        .find(|item| item.id == "evidence:ui")
        .unwrap();
    assert!(item.satisfied);
    assert_eq!(item.evidence_ids, vec![evidence.id.clone()]);

    let current =
        w.c.get_change(common::req(
            &w.tok("agent"),
            GetChangeRequest {
                repo: rref(&w.app),
                id: cr.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let second_head = w
        .vcs
        .create_commit(
            &w.app,
            NewCommit {
                parent: current.revisions.last().unwrap().head.clone(),
                edits: vec![
                    (
                        "web/settings.tsx".into(),
                        Some(
                            b"export const Settings = () => <h1 data-testid=\"settings-heading\">Account settings</h1>;\n"
                                .to_vec(),
                        ),
                    ),
                ],
                message: "add settings heading test id".into(),
                author: "agent".into(),
                creator: "agent".into(),
                chain: vec!["agent".into()],
                change_id: None,
            },
        )
        .await
        .unwrap();
    w.c.revise_change(common::req(
        &w.tok("agent"),
        ReviseChangeRequest {
            repo: rref(&w.app),
            id: cr.id.clone(),
            head: Some(second_head),
        },
    ))
    .await
    .unwrap();

    let listed =
        w.b.list_evidence(common::req(
            &w.tok("reader"),
            ListEvidenceRequest {
                repo: rref(&w.app),
                change_ids: vec![cr.id.clone()],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .evidence;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, evidence.id);
    assert!(
        listed[0].stale,
        "prior-revision evidence stays listed as stale"
    );

    let reopened = checklist(&w, &cr, 0).await;
    assert_eq!(reopened.revision, 2);
    let item = reopened
        .items
        .iter()
        .find(|item| item.id == "evidence:ui")
        .expect("the current UI revision must retain its checklist item");
    assert!(
        !item.satisfied,
        "stale evidence must not satisfy the current item"
    );
    assert!(item.evidence_ids.is_empty());
}
briefs_on_both!(stale_before_after_evidence_reopens_current_ui_checklist);
