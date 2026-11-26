//! GitHub migration scenarios (SPEC §17, AS-MIG-01) against an in-process fake of the GitHub
//! API and a local git repository: exact commit ids, preserved numbers, revisions from the
//! force-push timeline, review anchors, land-rule translation, placeholders and claims,
//! pagination, rate limits, resume after a crash, live sync, cutover and the fidelity report
//! validated against its schema.

use crate::common;
use std::time::Duration;

use bee_git::GitOid;
use bee_github::auth::{AppCredential, Credential};
use bee_github::live::{Delivery, sign, verify};
use bee_github::principals::{ClaimProof, principal_id};
use bee_github::report::{Status, category, reason};
use bee_github::sink::{ChangeEvent, PrState, ReviewState};
use bee_github::state::Phase;
use common::*;
use serde_json::json;

async fn commit_ids_identical(w: &World) {
    for sha in w.src.all_commits() {
        let oid = GitOid::parse(&sha).unwrap();
        let cs = bee_import::lookup_bee_id(w.stores.meta.as_ref(), w.stores.repo, &oid)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("commit {sha} was not imported"));
        let back = bee_import::lookup_git_id(w.stores.meta.as_ref(), w.stores.repo, &cs)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back.to_hex(), sha);
    }
}

#[tokio::test]
async fn as_mig_01_import_keeps_commit_ids_numbers_and_explains_nothing_dropped() {
    let w = World::new(false).await;
    let r = w.importer().run().await.unwrap();
    assert_eq!(r.phase, Phase::Mirror);
    // Git: every commit, including the force-pushed-away head, keeps its id; verification ran.
    commit_ids_identical(&w).await;
    let v = &r.report.git.verification;
    assert!(v.performed && v.passed(), "{v:?}");
    // Numbers are kept: PRs 2, 3, 5 and issues 1, 4.
    let c = w.sink.contents();
    assert_eq!(c.changes.keys().copied().collect::<Vec<_>>(), vec![2, 3, 5]);
    assert_eq!(c.issues.keys().copied().collect::<Vec<_>>(), vec![1, 4]);
    assert!(matches!(c.changes[&3].state, PrState::Merged { .. }));
    assert_eq!(c.changes[&5].state, PrState::Closed);
    assert_eq!(c.changes[&2].labels, vec!["enhancement".to_string()]);
    assert_eq!(c.changes[&2].milestone, Some(1));
    assert_eq!(c.issues[&4].state_reason.as_deref(), Some("not_planned"));
    // Links to this repository are rewritten, the original is kept.
    let ic = &c.issues[&1].comments[0];
    assert!(
        ic.body.body.contains("https://hub.test/acme/api/pull/2"),
        "{}",
        ic.body.body
    );
    assert!(
        ic.body
            .original
            .contains(&format!("{}/acme/api/pull/2", w.base))
    );
    // Releases with assets, labels, milestones, LFS, webhooks.
    assert_eq!(c.releases["v1.0"].assets[0].data, ASSET_DATA);
    assert_eq!(c.labels.len(), 2);
    assert_eq!(c.milestones[&1].title, "v1");
    assert_eq!(
        c.lfs.get(&format!("sha256:{}", lfs_oid())),
        Some(&(LFS_DATA.len() as u64))
    );
    assert_eq!(c.webhooks[&70].url, "https://ci.example/hook");
    // The report: complete, nothing unexplained, valid against the schema.
    assert!(r.report.complete(), "{}", r.report.human());
    assert_eq!(r.report.git.summary, "imported 100%");
    assert_eq!(r.report.github.unexplained_drops, 0);
    assert_eq!(
        r.report.category(category::PULL_REQUESTS).unwrap().imported,
        3
    );
    assert_eq!(r.report.category(category::ISSUES).unwrap().imported, 2);
    assert_eq!(
        r.report.category(category::REVIEWS).unwrap().excluded,
        1,
        "the pending review"
    );
    assert_eq!(r.report.category(category::SECRETS).unwrap().archived, 1);
    assert_eq!(r.report.category(category::WORKFLOWS).unwrap().imported, 1);
    assert!(
        r.report
            .github
            .manual_steps
            .iter()
            .any(|s| s.contains("NPM_TOKEN"))
    );
    validate_report(&r.report.to_value());
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(w.state.path().join("report.json")).unwrap())
            .unwrap();
    assert_eq!(on_disk, r.report.to_value());
}

#[tokio::test]
async fn as_mig_drops_are_reported_with_reasons() {
    let w = World::new(true).await;
    let r = w.importer().run().await.unwrap();
    assert!(!r.report.complete());
    let drops: Vec<(String, String)> = r
        .report
        .git
        .categories
        .iter()
        .flat_map(|c| {
            c.drops
                .iter()
                .map(move |d| (c.category.clone(), d.reason.clone()))
        })
        .collect();
    assert!(
        drops.contains(&(
            category::FORCE_PUSH_COMMITS.into(),
            reason::FORCE_PUSH_COMMIT_UNREACHABLE.into()
        )),
        "{drops:?}"
    );
    assert!(
        drops.contains(&(
            category::BRANCH_PROTECTION.into(),
            reason::UNTRANSLATABLE.into()
        )),
        "{drops:?}"
    );
    assert!(
        drops.contains(&(category::WEBHOOKS.into(), reason::UNREADABLE.into())),
        "{drops:?}"
    );
    assert!(
        r.report
            .git
            .categories
            .iter()
            .all(|c| c.dropped == c.drops.len() as u64)
    );
    assert!(r.report.git.summary.starts_with("incomplete: "));
    assert_eq!(r.report.github.unexplained_drops, 0);
    validate_report(&r.report.to_value());
}

#[tokio::test]
async fn as_mig_force_push_timeline_becomes_revisions() {
    let w = World::new(true).await;
    w.importer().run().await.unwrap();
    let c = w.sink.contents();
    let pr2 = &c.changes[&2];
    let heads: Vec<&str> = pr2.revisions.iter().map(|r| r.head.as_str()).collect();
    assert_eq!(
        heads,
        vec![w.src.sha("c2a").as_str(), w.src.sha("c2").as_str()]
    );
    assert!(!pr2.revisions[0].forced && pr2.revisions[1].forced);
    assert_eq!(pr2.revisions[1].pusher.as_ref().unwrap().github_id, ALICE);
    // The old head is pinned and imported (`pull/2/rev/1`), the base too.
    let refs = bee_github::git::Mirror::open(&w.state.path().join("mirror.git"), None)
        .unwrap()
        .refs()
        .unwrap();
    assert!(
        refs.iter()
            .any(|(n, s)| n == "refs/pull/2/rev/1" && *s == w.src.sha("c2a"))
    );
    assert!(refs.iter().any(|(n, _)| n == "refs/pull/3/base"));
    // PR 5's pre-force-push head is gone from GitHub: one revision and a drop.
    assert_eq!(c.changes[&5].revisions.len(), 1);
}

#[tokio::test]
async fn as_mig_review_comments_keep_their_anchors_and_threads() {
    let w = World::new(false).await;
    w.importer().run().await.unwrap();
    let pr2 = &w.sink.contents().changes[&2];
    let mut seen = 0;
    for e in &pr2.events {
        if let ChangeEvent::ReviewComment {
            id,
            path,
            side,
            line,
            commit,
            in_reply_to,
            diff_hunk,
            file_level,
            ..
        } = e
        {
            seen += 1;
            assert_eq!(path, "src/lib.rs");
            assert_eq!(side, "new");
            assert_eq!(*commit, w.src.sha("c2"));
            assert!(diff_hunk.starts_with("@@"));
            assert!(!file_level);
            match id {
                100 => assert_eq!(*line, Some(2)),
                101 => assert_eq!(*in_reply_to, Some(100)),
                102 => assert_eq!(*line, Some(999)),
                _ => panic!("unexpected comment {id}"),
            }
        }
    }
    assert_eq!(seen, 3);
    // Events are in time order and reviews map their verdicts.
    let ats: Vec<&str> = pr2.events.iter().map(|e| e.at()).collect();
    let mut sorted = ats.clone();
    sorted.sort();
    assert_eq!(ats, sorted);
    assert!(pr2.events.iter().any(|e| matches!(
        e,
        ChangeEvent::Review {
            id: 900,
            state: ReviewState::Approved,
            ..
        }
    )));
}

#[tokio::test]
async fn as_mig_branch_protection_becomes_land_rules() {
    let w = World::new(true).await;
    let r = w.importer().run().await.unwrap();
    let toml = std::fs::read_to_string(w.state.path().join("policy.toml")).unwrap();
    assert_eq!(toml, r.policy.toml);
    assert!(toml.contains("[[land]]\nref = \"main\""));
    assert!(toml.contains("required_checks = [\"ci/test\"]"));
    assert!(toml.contains("dismiss_stale_approvals = true"));
    assert!(toml.contains("from = \"owners\""));
    assert!(toml.contains("codeowners = \".github/CODEOWNERS\""));
    assert!(toml.contains("delete = []"));
    bee_policy::parse_doc(&toml, bee_policy::Origin::Repo).unwrap();
    // The policy the sink got for review is the same text.
    assert_eq!(w.sink.contents().policy.unwrap().toml, toml);
    // The glob pattern and the CODEOWNERS character class are listed, not silently lost.
    let settings: Vec<&str> = r
        .report
        .github
        .untranslatable
        .iter()
        .map(|u| u.setting.as_str())
        .collect();
    assert!(
        settings.contains(&"branch_protection.pattern"),
        "{settings:?}"
    );
    assert!(settings.contains(&"codeowners.line"), "{settings:?}");
    let co = r.report.category(category::CODEOWNERS).unwrap();
    assert_eq!((co.imported, co.dropped), (2, 1));
}

#[tokio::test]
async fn as_mig_placeholders_are_claimable_across_repos() {
    let w = World::new(false).await;
    let imp = w.importer();
    let r = imp.run().await.unwrap();
    let ph = &r.report.github.placeholders;
    let alice = ph.iter().find(|p| p.github_id == ALICE).unwrap();
    assert_eq!(alice.principal, principal_id(ALICE));
    assert!(!alice.claimed && alice.contributions >= 2);
    assert_eq!(
        ph.iter().find(|p| p.github_id == BOT).unwrap().kind,
        "agent"
    );
    assert!(
        w.sink
            .contents()
            .principals
            .contains_key(&principal_id(BOB))
    );
    // Tokens never appear in the report.
    let token = imp.claim_token(ALICE).unwrap();
    assert!(!r.report.to_json().contains(&token));
    // A wrong proof is refused; the right token moves alice's history to her account.
    let e = imp
        .claim(ALICE, &ClaimProof::Token("nope".into()), "01REALALICE")
        .await
        .unwrap_err();
    assert_eq!(e.reason, bee_core::Reason::PermissionDenied);
    let (claim, moved) = imp
        .claim(ALICE, &ClaimProof::Token(token), "01REALALICE")
        .await
        .unwrap();
    assert_eq!(claim.placeholder, principal_id(ALICE));
    assert!(moved >= 2);
    let c = w.sink.contents();
    assert_eq!(c.changes[&2].author.principal, "01REALALICE");
    // Placeholder ids depend only on the GitHub id: another repo's import gives alice the
    // same principal, so one claim covers it too.
    assert_eq!(
        principal_id(ALICE),
        bee_github::principals::Placeholder::of(&bee_github::model::User {
            id: ALICE,
            login: "renamed".into(),
            ..Default::default()
        })
        .principal
    );
    // Re-running after the claim attributes new records to the claimant.
    let r2 = imp.run().await.unwrap();
    assert!(
        r2.report
            .github
            .placeholders
            .iter()
            .find(|p| p.github_id == ALICE)
            .unwrap()
            .claimed
    );
}

#[tokio::test]
async fn as_mig_pagination_follows_link_headers() {
    let w = World::new(false).await;
    let mut cfg = w.config();
    cfg.per_page = 2;
    let r = w.importer_with(cfg, w.options()).run().await.unwrap();
    assert!(r.report.complete(), "{}", r.report.human());
    let c = w.sink.contents();
    assert_eq!(c.changes.len(), 3);
    assert_eq!(c.issues.len(), 2);
    // 5 issue-list entries at 2 per page is 3 pages.
    assert_eq!(w.fake.hits(&format!("/repos/{FULL}/issues")), 3);
}

#[tokio::test]
async fn as_mig_rate_limits_wait_and_retry() {
    let w = World::new(false).await;
    w.fake.faults.lock().unwrap().primary_once = Some(30);
    w.fake.faults.lock().unwrap().secondary_once = Some("/pulls/comments".into());
    let imp = w.importer();
    let r = imp.run().await.unwrap();
    assert!(r.report.complete());
    let st = imp.client().stats();
    assert_eq!(st.rate_limit_waits, 1, "{st:?}");
    assert_eq!(st.secondary_limit_waits, 1, "{st:?}");
    let sleeps = w.clock.sleeps();
    assert!(sleeps.contains(&Duration::from_secs(31)), "{sleeps:?}");
    assert!(sleeps.contains(&Duration::from_secs(7)), "{sleeps:?}");
}

#[tokio::test]
async fn as_mig_rate_limit_beyond_the_wait_budget_is_resource_exhausted() {
    let w = World::new(false).await;
    w.fake.faults.lock().unwrap().primary_once = Some(7200);
    let mut cfg = w.config();
    cfg.max_wait_secs = 600;
    let e = w.importer_with(cfg, w.options()).run().await.unwrap_err();
    assert_eq!(e.reason, bee_core::Reason::ResourceExhausted);
    assert!(e.payload.contains_key("retry_after_secs"));
}

#[tokio::test]
async fn as_mig_resume_after_crash_does_not_refetch_or_duplicate() {
    let w = World::new(false).await;
    let mut cfg = w.config();
    cfg.per_page = 1;
    cfg.max_retries = 0;
    // Crash part-way through the metadata.
    w.fake.faults.lock().unwrap().fail_after = Some(9);
    let imp = w.importer_with(cfg.clone(), w.options());
    assert!(imp.run().await.is_err());
    let labels_before = w.fake.hits(&format!("/repos/{FULL}/labels"));
    assert!(labels_before > 0);
    w.fake.faults.lock().unwrap().fail_after = None;
    let imp = w.importer_with(cfg.clone(), w.options());
    let r = imp.run().await.unwrap();
    assert!(r.report.complete(), "{}", r.report.human());
    // Pages stored before the crash were not fetched again.
    assert_eq!(
        w.fake.hits(&format!("/repos/{FULL}/labels")),
        2,
        "two label pages, each fetched once"
    );
    // A third run places nothing new and fetches no list again.
    let issues_hits = w.fake.hits(&format!("/repos/{FULL}/issues"));
    let r3 = w.importer_with(cfg, w.options()).run().await.unwrap();
    assert!(r3.placed.is_empty(), "{:?}", r3.placed);
    assert_eq!(w.fake.hits(&format!("/repos/{FULL}/issues")), issues_hits);
    for (k, n) in w.sink.contents().writes {
        assert_eq!(n, 1, "{k} placed {n} times");
    }
    // Same counts and drops; only per-run facts (ref actions, walk stats) differ.
    assert!(
        r3.report.git.categories == r.report.git.categories,
        "categories changed on a no-op run"
    );
    assert!(
        r3.report.github == r.report.github,
        "github section changed on a no-op run"
    );
}

#[tokio::test]
async fn as_mig_github_app_auth_uses_installation_tokens() {
    let w = World::new(false).await;
    let pem = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/app-key.pem"
    ))
    .unwrap();
    let app = AppCredential::new("4242", &pem, 99).unwrap();
    *w.fake.app_public_key.lock().unwrap() = Some(app.public_key_der());
    let client = w.client_with(w.config(), Credential::App(app));
    let imp = bee_github::Importer::new(
        client,
        bee_github::state::StateDir::open(w.state.path()).unwrap(),
        w.stores.target(),
        w.sink.clone(),
        w.options(),
    );
    let r = imp.run().await.unwrap();
    assert!(r.report.complete(), "{}", r.report.human());
    assert_eq!(
        w.fake.hits("/app/installations/99/access_tokens"),
        1,
        "token cached"
    );
}

#[tokio::test]
async fn as_mig_webhooks_keep_the_mirror_current() {
    let w = World::new(false).await;
    let imp = w.importer();
    imp.run().await.unwrap();
    let secret = b"hook-secret";
    let body = serde_json::to_vec(&json!({
        "action": "created",
        "issue": { "number": 1, "url": format!("{}/repos/{FULL}/issues/1", w.base), "title": "Crash on start" },
        "comment": { "id": 250, "user": user(DAVE, "dave"), "body": "me too", "created_at": "2026-02-01T00:00:00Z", "updated_at": "2026-02-01T00:00:00Z" },
    }))
    .unwrap();
    let sig = sign(secret, &body);
    let headers = |k: &str| match k {
        "x-hub-signature-256" => Some(sig.clone()),
        "x-github-event" => Some("issue_comment".to_string()),
        "x-github-delivery" => Some("delivery-1".to_string()),
        _ => None,
    };
    let d = verify(secret, &headers, &body).unwrap();
    let res = imp.webhook(&d).await.unwrap();
    assert!(res.placed.contains(&"issue:1".to_string()), "{res:?}");
    let c = w.sink.contents();
    assert!(c.issues[&1].comments.iter().any(|x| x.id == 250));
    // Duplicate deliveries are ignored; forged ones never get in.
    assert_eq!(
        imp.webhook(&d).await.unwrap().ignored.as_deref(),
        Some("duplicate delivery")
    );
    assert!(verify(b"wrong", &headers, &body).is_err());
    // A PR push refetches git.
    let push = Delivery {
        id: "delivery-2".into(),
        event: "push".into(),
        payload: json!({ "ref": "refs/heads/main" }),
    };
    assert!(imp.webhook(&push).await.unwrap().git_synced);
    // The HTTP endpoint: 401 for a bad signature.
    let app = bee_github::live::webhook_router(bee_github::live::WebhookApp {
        importer: std::sync::Arc::new(w.importer()),
        secret: std::sync::Arc::new(secret.to_vec()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = w.client_with(
        bee_github::ClientConfig::at(&format!("http://{addr}")),
        Credential::Anonymous,
    );
    let bad = client
        .post_raw(
            &format!("http://{addr}/webhooks/github"),
            &json!({}),
            &[("x-hub-signature-256", "sha256=00".into())],
            false,
        )
        .await
        .unwrap();
    assert_eq!(bad.status, 401);
}

#[tokio::test]
async fn as_mig_polling_repairs_missed_changes_with_conditional_requests() {
    let w = World::new(false).await;
    let imp = w.importer();
    imp.run().await.unwrap();
    imp.sync().await.unwrap();
    // An issue edited on GitHub with no webhook delivered.
    {
        let mut d = w.fake.data.lock().unwrap();
        let i = d
            .issues
            .iter_mut()
            .find(|i| i["number"] == json!(4))
            .unwrap();
        i["title"] = json!("Wontfix (renamed)");
        i["updated_at"] = json!("2026-03-01T00:00:00Z");
    }
    let r = imp.sync().await.unwrap();
    assert!(r.placed.contains(&"issue:4".to_string()), "{r:?}");
    assert_eq!(w.sink.contents().issues[&4].title, "Wontfix (renamed)");
    // Nothing changed since: the lists answer 304 and nothing is placed.
    imp.sync().await.unwrap();
    let before = imp.client().stats().not_modified;
    let r = imp.sync().await.unwrap();
    assert!(r.placed.is_empty(), "{r:?}");
    assert!(imp.client().stats().not_modified > before);
}

#[tokio::test]
async fn as_mig_cutover_freezes_and_writes_the_redirect_map() {
    let w = World::new(false).await;
    let imp = w.importer();
    imp.run().await.unwrap();
    let r = imp.cutover(true).await.unwrap();
    assert_eq!(r.phase, Phase::CutOver);
    assert_eq!(r.report.github.phase, "cut_over");
    assert!(*w.fake.archived.lock().unwrap(), "GitHub repo archived");
    let html = format!("{}/{FULL}", w.base);
    assert_eq!(
        r.redirects.resolve(&format!("{html}/pull/2")).unwrap(),
        "https://hub.test/acme/api/pull/2"
    );
    assert_eq!(
        r.redirects.resolve(&format!("{html}/issues/4")).unwrap(),
        "https://hub.test/acme/api/issues/4"
    );
    assert_eq!(
        r.redirects
            .resolve(&format!("{html}/pull/3/files"))
            .unwrap(),
        "https://hub.test/acme/api/pull/3/files"
    );
    assert_eq!(
        r.redirects
            .resolve(&format!("{html}/releases/tag/v1.0"))
            .unwrap(),
        "https://hub.test/acme/api/releases/tag/v1.0"
    );
    assert_eq!(w.sink.contents().redirects.unwrap(), r.redirects);
    let file: bee_github::links::RedirectMap = serde_json::from_str(
        &std::fs::read_to_string(w.state.path().join("redirects.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(file, r.redirects);
    // After cutover GitHub changes are refused.
    let d = Delivery {
        id: "late".into(),
        event: "push".into(),
        payload: json!({}),
    };
    assert_eq!(
        imp.webhook(&d).await.unwrap_err().reason,
        bee_core::Reason::InvalidState
    );
    assert_eq!(
        imp.run().await.unwrap_err().reason,
        bee_core::Reason::InvalidState
    );
    assert!(r.report.complete());
    validate_report(&r.report.to_value());
}

#[tokio::test]
async fn as_mig_mirror_only_stops_after_git() {
    let w = World::new(false).await;
    let mut o = w.options();
    o.mirror_only = true;
    let r = w.importer_with(w.config(), o).run().await.unwrap();
    assert_eq!(r.report.github.phase, "mirror_only");
    commit_ids_identical(&w).await;
    assert!(w.sink.contents().changes.is_empty());
    assert_eq!(w.fake.hits(&format!("/repos/{FULL}/pulls")), 0);
    validate_report(&r.report.to_value());
}

#[tokio::test]
async fn as_mig_dry_run_estimates_without_fetching() {
    let w = World::new(false).await;
    let e = w.importer().dry_run().await.unwrap();
    assert_eq!((e.pull_requests, e.issues), (3, 2));
    assert!(e.requests > 0 && e.rate_limit == 5000);
    assert_eq!(w.fake.hits(&format!("/repos/{FULL}/issues")), 0);
}

#[tokio::test]
async fn as_mig_org_migration_archive_is_a_metadata_source() {
    let w = World::new(false).await;
    // Build an archive with the repository's git data and metadata in archive form.
    let bare = tempfile::tempdir().unwrap();
    let bare_repo = bare.path().join("api.git");
    git(
        bare.path(),
        &[
            "clone",
            "-q",
            "--mirror",
            &w.src.path().display().to_string(),
            &bare_repo.display().to_string(),
        ],
    );
    let mut entries = Vec::new();
    for e in walk(&bare_repo) {
        let rel = e.strip_prefix(&bare_repo).unwrap().display().to_string();
        let path = format!("repositories/{FULL}.git/{rel}");
        if e.is_dir() {
            entries.push(bee_github::archive::TarEntry {
                path: format!("{path}/"),
                dir: true,
                data: vec![],
            });
        } else {
            entries.push(bee_github::archive::TarEntry {
                path,
                dir: false,
                data: std::fs::read(&e).unwrap(),
            });
        }
    }
    let gh = format!("https://github.com/{FULL}");
    let files = [
        (
            "repositories_000001.json",
            json!([{ "type": "repository", "url": gh, "labels": [{ "name": "bug", "color": "d73a4a" }] }]),
        ),
        (
            "users_000001.json",
            json!([{ "type": "user", "url": "https://github.com/alice", "login": "alice" }, { "type": "user", "url": "https://github.com/bob", "login": "bob" }]),
        ),
        (
            "pull_requests_000001.json",
            json!([{ "type": "pull_request", "url": format!("{gh}/pull/2"), "repository": gh, "user": "https://github.com/alice", "title": "Add feature", "body": "b",
            "head": { "ref": "feat", "sha": w.src.sha("c2") }, "base": { "ref": "main", "sha": w.src.sha("c1") }, "labels": [format!("{gh}/labels/bug")], "created_at": "2026-01-02T00:00:00Z" }]),
        ),
        (
            "issues_000001.json",
            json!([{ "type": "issue", "url": format!("{gh}/issues/1"), "repository": gh, "user": "https://github.com/bob", "title": "Crash on start", "body": "x", "created_at": "2026-01-01T00:00:00Z" }]),
        ),
        (
            "issue_comments_000001.json",
            json!([{ "type": "issue_comment", "url": format!("{gh}/issues/1#issuecomment-200"), "issue": format!("{gh}/issues/1"), "user": "https://github.com/alice", "body": "hi", "created_at": "2026-01-02T00:00:00Z" }]),
        ),
        (
            "pull_request_reviews_000001.json",
            json!([{ "type": "pull_request_review", "url": format!("{gh}/pull/2/files#pullrequestreview-900"), "pull_request": format!("{gh}/pull/2"), "user": "https://github.com/bob", "body": "ok", "state": 40, "head_sha": w.src.sha("c2"), "submitted_at": "2026-01-04T00:00:00Z" }]),
        ),
    ];
    for (name, v) in files {
        entries.push(bee_github::archive::TarEntry {
            path: name.into(),
            dir: false,
            data: serde_json::to_vec(&v).unwrap(),
        });
    }
    let tar = bee_github::archive::write_tar(&entries);
    let mut gz = Vec::new();
    {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
        enc.write_all(&tar).unwrap();
        enc.finish().unwrap();
    }
    // Fetch it the way GitHub serves it: start, poll, download through a redirect.
    w.fake.data.lock().unwrap().migration_archive = gz;
    let client = w.client_with(w.config(), Credential::Token("t".into()));
    let id = bee_github::archive::start_org_migration(&client, OWNER, &[NAME.to_string()])
        .await
        .unwrap();
    bee_github::archive::wait_org_migration(&client, OWNER, id, Duration::from_secs(5), 10)
        .await
        .unwrap();
    let path = w.state.path().join("download.tar.gz");
    bee_github::archive::download_org_migration(&client, OWNER, id, &path)
        .await
        .unwrap();
    // Only the archive provides lists: the API list endpoints return nothing.
    {
        let mut d = w.fake.data.lock().unwrap();
        d.pulls.clear();
        d.issues.clear();
        d.issue_comments.clear();
        d.review_comments.clear();
    }
    let mut o = w.options();
    o.archive = Some(path);
    let r = w.importer_with(w.config(), o).run().await.unwrap();
    commit_ids_identical(&w).await;
    let c = w.sink.contents();
    assert_eq!(c.changes.keys().copied().collect::<Vec<_>>(), vec![2]);
    assert_eq!(
        c.issues[&1].comments[0].author.github_id, ALICE,
        "logins resolved to ids through the API"
    );
    assert!(c.changes[&2].events.iter().any(|e| matches!(
        e,
        ChangeEvent::Review {
            id: 900,
            state: ReviewState::Approved,
            ..
        }
    )));
    assert!(
        r.report
            .github
            .sources
            .contains(&"migration_archive".to_string())
    );
    assert_eq!(
        w.fake.hits(&format!("/repos/{FULL}/pulls")),
        0,
        "archive steps skip the API lists"
    );
    validate_report(&r.report.to_value());
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        out.push(p.clone());
        if p.is_dir() {
            out.extend(walk(&p));
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn as_mig_outcomes_cover_every_record() {
    let w = World::new(true).await;
    let r = w.importer().run().await.unwrap();
    // Every review comment, review and conversation comment has exactly one outcome.
    let rc = r.report.category(category::REVIEW_COMMENTS).unwrap();
    assert_eq!(rc.imported + rc.archived + rc.dropped, 3);
    let ic = r.report.category(category::ISSUE_COMMENTS).unwrap();
    assert_eq!(ic.imported + ic.archived + ic.dropped, 2);
    let rv = r.report.category(category::REVIEWS).unwrap();
    assert_eq!(rv.imported + rv.archived + rv.dropped + rv.excluded, 4);
    let rx = r.report.category(category::REACTIONS).unwrap();
    assert_eq!(rx.imported, 3);
    let _ = Status::Imported;
}

/// Imports terramind-org/bee from github.com with `gh auth token` (opt-in:
/// `BEE_TEST_GITHUB=1`). Checks every commit keeps its id and every drop has a reason.
#[tokio::test]
async fn as_mig_live_github_import() {
    if std::env::var("BEE_TEST_GITHUB").as_deref() != Ok("1") {
        return;
    }
    let (token, _) = bee_github::auth::discover_token("github.com").expect("gh auth token");
    let st = stores().await;
    let state = tempfile::tempdir().unwrap();
    let sink = std::sync::Arc::new(bee_github::MemorySink::new());
    let client = bee_github::Client::new(
        bee_github::ClientConfig::default(),
        Credential::Token(token),
        std::sync::Arc::new(bee_github::clock::SystemClock),
    );
    let repo = std::env::var("BEE_TEST_GITHUB_REPO").unwrap_or_else(|_| "terramind-org/bee".into());
    let imp = bee_github::Importer::new(
        client,
        bee_github::state::StateDir::open(state.path()).unwrap(),
        st.target(),
        sink,
        bee_github::Options::new(repo),
    );
    let r = imp.run().await.unwrap();
    assert!(r.report.git.verification.passed());
    assert_eq!(r.report.github.unexplained_drops, 0);
    for c in &r.report.git.categories {
        for d in &c.drops {
            assert!(!d.reason.is_empty());
        }
    }
    validate_report(&r.report.to_value());
}
