//! `ImportService.StartImport` with `source_kind = GITHUB` on the real server, against the
//! in-process fake GitHub API of `bee-github`'s tests: commits keep their git ids (the
//! server's git gateway advertises the same ids as the source), change requests and issues
//! keep their GitHub numbers in the real forge, and placeholders are listed.

mod common;

#[allow(unused_imports)]
#[path = "../../bee-github/tests/common/mod.rs"]
mod gh;

use std::collections::BTreeMap;

use common::*;
use serde_json::json;

/// Refs to the commits they name (annotated tags peeled: the gateway advertises imported
/// annotated tags as lightweight tags of their commit, `docs/spec/git-gateway.md` §4).
fn show_refs(out: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut peeled = Vec::new();
    for l in out.lines() {
        let Some((sha, name)) = l.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim();
        if !(name.starts_with("refs/heads/")
            || name.starts_with("refs/tags/")
            || name.starts_with("refs/pull/"))
        {
            continue;
        }
        match name.strip_suffix("^{}") {
            Some(base) => peeled.push((base.to_string(), sha.to_string())),
            None => {
                m.insert(name.to_string(), sha.to_string());
            }
        }
    }
    m.extend(peeled);
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_github_import_keeps_ids_and_numbers_in_the_real_forge() {
    let w = gh::World::new(false).await;
    let base = w.base.clone();
    let t = start_with(move |c| {
        c.import.github_url = base;
        c.import.github_sync_interval_secs = 0;
        c.import.github_webhook_secret = "hook-secret".into();
    })
    .await;
    t.call("OrgService", "CreateOrg", json!({ "name": gh::OWNER }))
        .await
        .expect("target org");
    let started = t
        .call(
            "ImportService",
            "StartImport",
            json!({
                "source": gh::FULL, "sourceKind": "IMPORT_SOURCE_KIND_GITHUB",
                "githubToken": "ghp_test", "phase": "IMPORT_PHASE_MIRROR",
            }),
        )
        .await
        .expect("start");
    let id = started["id"].as_str().unwrap().to_string();
    let done = eventually(240, "the GitHub import to finish", || async {
        let g = t
            .call("ImportService", "GetImport", json!({ "id": id }))
            .await
            .ok()?;
        match g["state"].as_str() {
            Some("IMPORT_STATE_LIVE") => Some(Ok(g)),
            Some("IMPORT_STATE_FAILED") => Some(Err(g)),
            _ => None,
        }
    })
    .await
    .unwrap_or_else(|g| panic!("import failed: {g}"));
    assert_eq!(done["repoIds"].as_array().unwrap().len(), 1);

    // Identical git ids: the server's git gateway advertises what GitHub had.
    let src = gh::git(&w.src.path(), &["show-ref", "-d"]);
    let enc: String = t
        .token
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let url = format!("http://git:{enc}@{}/{}.git", t.srv.addr, gh::FULL);
    let fetch_url = url.clone();
    let dir = w.src.path();
    let remote = tokio::task::spawn_blocking(move || gh::git(&dir, &["ls-remote", &url]))
        .await
        .unwrap();
    let (want, got) = (show_refs(&src), show_refs(&remote));
    for (name, sha) in &want {
        assert_eq!(
            got.get(name),
            Some(sha),
            "{name}: source {want:#?}\nserver {got:#?}"
        );
    }
    // A pull request ref is fetchable by name over the gateway (`refs/pull/N/head`).
    let (pull_name, pull_sha) = want
        .iter()
        .find(|(n, _)| n.starts_with("refs/pull/") && n.ends_with("/head"))
        .map(|(n, s)| (n.clone(), s.clone()))
        .expect("the source has pull request refs");
    let fetch_dir = tempfile::tempdir().unwrap();
    let fd = fetch_dir.path().to_path_buf();
    let fetched = tokio::task::spawn_blocking(move || {
        gh::git(&fd, &["init", "-q"]);
        gh::git(&fd, &["fetch", "-q", &fetch_url, &pull_name]);
        gh::git(&fd, &["rev-parse", "FETCH_HEAD"])
    })
    .await
    .unwrap();
    assert_eq!(fetched.trim(), pull_sha);

    // Numbers survive in the forge: PRs as change requests, issues in the work domain.
    let repo = json!({ "fullName": gh::FULL });
    for (n, state) in [
        (2, "CHANGE_STATE_OPEN"),
        (3, "CHANGE_STATE_CLOSED"),
        (5, "CHANGE_STATE_CLOSED"),
    ] {
        let cr = t
            .call(
                "ChangeService",
                "GetChangeByNumber",
                json!({ "repo": repo, "number": n }),
            )
            .await
            .unwrap_or_else(|e| panic!("CR #{n}: {e:?}"));
        assert_eq!(cr["number"], n.to_string());
        assert_eq!(cr["state"], state, "#{n}");
    }
    let i1 = t
        .call(
            "IssueService",
            "GetIssueByNumber",
            json!({ "repo": repo, "number": 1 }),
        )
        .await
        .expect("issue #1");
    assert_eq!(i1["title"], "Crash on start");

    // The report and the placeholders are served.
    let report = t
        .call("ImportService", "GetFidelityReport", json!({ "id": id }))
        .await
        .expect("report");
    assert!(
        report["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["category"] == "pull_requests"),
        "{report}"
    );
    let ph = t
        .call(
            "ImportService",
            "ListPlaceholderPrincipals",
            json!({ "importId": id }),
        )
        .await
        .expect("placeholders");
    assert!(!ph["placeholders"].as_array().unwrap().is_empty(), "{ph}");

    // Live sync: a signed GitHub webhook for a new issue lands it under its number.
    let payload = json!({
        "action": "opened",
        "repository": { "full_name": gh::FULL },
        "sender": gh::user(gh::CAROL, "carol"),
        "issue": {
            "number": 9, "title": "Reported live", "body": "seen after the import",
            "state": "open", "state_reason": null, "user": gh::user(gh::CAROL, "carol"),
            "labels": [], "assignees": [], "milestone": null, "locked": false,
            "created_at": "2026-02-01T00:00:00Z", "updated_at": "2026-02-01T00:00:00Z",
            "closed_at": null, "reactions": { "total_count": 0 },
        },
    })
    .to_string();
    let sig = bee_github::live::sign(b"hook-secret", payload.as_bytes());
    let url = format!("{}/webhooks/github", t.url());
    let headers = [
        ("content-type", "application/json"),
        ("x-github-event", "issues"),
        ("x-github-delivery", "delivery-1"),
        ("x-hub-signature-256", sig.as_str()),
    ];
    let (st, _, body) = http("POST", &url, &headers, payload.clone().into_bytes()).await;
    assert_eq!(st, 200, "{}", String::from_utf8_lossy(&body));
    let i9 = t
        .call(
            "IssueService",
            "GetIssueByNumber",
            json!({ "repo": repo, "number": 9 }),
        )
        .await
        .expect("issue #9 from the webhook");
    assert_eq!(i9["title"], "Reported live");
    let bad = [
        ("content-type", "application/json"),
        ("x-github-event", "issues"),
        ("x-github-delivery", "delivery-2"),
        ("x-hub-signature-256", "sha256=00"),
    ];
    let (st, _, _) = http("POST", &url, &bad, payload.into_bytes()).await;
    assert_eq!(st, 401, "a bad signature is refused");
}
