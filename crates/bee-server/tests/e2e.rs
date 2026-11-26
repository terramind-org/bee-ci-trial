//! End-to-end tests over real HTTP/JSON and gRPC against an in-process server (SQLite + disk;
//! Postgres when `BEE_TEST_PG` is set).

mod common;

use base64::Engine as _;
use bee_proto::v1::{self, error_detail::Payload};
use common::*;
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_core_01_commit_and_land_one_file_fix_through_the_api() {
    let t = start_default().await;
    let (root, v) = t.tip("main").await;
    assert_eq!(v, 1);
    assert_eq!(root, bee_object::Changeset::empty_root().id().to_text());
    let cs = t
        .commit(&root, &[("src/lib.rs", b"fn a() {}\n")], 1, "add lib")
        .await;
    let head = id_text(&cs["id"]);
    let landed = t.land(&head).await.expect("land");
    assert_eq!(landed["ref"]["version"], "2");
    assert!(landed["landed"][0]["skipped"].is_null());
    // "Committed, response lost": the retry returns the recorded result.
    let again = t.land(&head).await.expect("retry");
    assert_eq!(again["ref"]["version"], "2");
    assert_eq!(again["landed"][0]["skipped"], true);
    // The fix.
    let (tip, _) = t.tip("main").await;
    let fix = t
        .commit(&tip, &[("src/lib.rs", b"fn a() { fixed() }\n")], 2, "fix a")
        .await;
    let fix_id = id_text(&fix["id"]);
    let out = t.land(&fix_id).await.expect("land fix");
    assert_eq!(out["ref"]["version"], "3");
    let (tip, v) = t.tip("main").await;
    assert_eq!((tip.as_str(), v), (fix_id.as_str(), 3));
    assert_eq!(
        t.read("src/lib.rs", "main").await.unwrap(),
        b"fn a() { fixed() }\n"
    );
    // Log, diff and blame answer from the API alone.
    let log = t
        .call(
            "QueryService",
            "Log",
            json!({ "repo": repo(), "from": { "expr": "main" }, "includeMeta": true }),
        )
        .await
        .unwrap();
    let msgs: Vec<&str> = log["changesets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["meta"]["message"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(msgs, vec!["fix a", "add lib", ""]);
    let diff = t
        .call("QueryService", "Diff", json!({ "repo": repo(), "from": { "expr": "main~1" }, "to": { "expr": "main" }, "includeHunks": true }))
        .await
        .unwrap();
    let item = &diff["items"][0];
    assert_eq!(item["status"], "DIFF_STATUS_MODIFIED");
    let lines: Vec<String> = item["hunks"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect();
    assert!(
        lines.contains(&"+fn a() { fixed() }\n".to_string()),
        "{lines:?}"
    );
    assert!(lines.contains(&"-fn a() {}\n".to_string()), "{lines:?}");
    let blame = t
        .call(
            "QueryService",
            "Blame",
            json!({ "repo": repo(), "at": { "expr": "main" }, "path": B64.encode("src/lib.rs") }),
        )
        .await
        .unwrap();
    assert_eq!(id_text(&blame["ranges"][0]["changeset"]), fix_id);
    // Ancestry, merge base, revisions, listing, changesets and derived data.
    let anc = t
        .call("QueryService", "IsAncestor", json!({ "repo": repo(), "ancestor": { "expr": "main~2" }, "descendant": { "expr": "main" } }))
        .await
        .unwrap();
    assert_eq!(anc["isAncestor"], true);
    let mb = t
        .call(
            "QueryService",
            "MergeBase",
            json!({ "repo": repo(), "a": { "expr": "main" }, "b": { "expr": "main~1" } }),
        )
        .await
        .unwrap();
    assert_eq!(id_text(&mb["bases"][0]), id_text(&cs["id"]));
    let res = t
        .call(
            "QueryService",
            "Resolve",
            json!({ "repo": repo(), "revisions": [{ "expr": "main@2" }, { "expr": "nope" }] }),
        )
        .await
        .unwrap();
    assert_eq!(id_text(&res["ids"][0]), head);
    assert_eq!(
        res["statuses"],
        json!(["ITEM_STATUS_OK", "ITEM_STATUS_NOT_FOUND"])
    );
    let tree = t
        .call(
            "QueryService",
            "ListTree",
            json!({ "repo": repo(), "at": { "expr": "main" }, "recursive": true }),
        )
        .await
        .unwrap();
    let paths: Vec<String> = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| String::from_utf8(B64.decode(e["path"].as_str().unwrap()).unwrap()).unwrap())
        .collect();
    assert_eq!(paths, vec!["src", "src/lib.rs"]);
    let got = t
        .call(
            "QueryService",
            "GetChangesets",
            json!({ "repo": repo(), "ids": [id_json(&fix_id)], "includeMeta": true }),
        )
        .await
        .unwrap();
    assert_eq!(got["changesets"][0]["meta"]["message"], "fix a");
    assert_eq!(got["results"][0]["status"], "ITEM_STATUS_OK");
    let derived = t
        .call("QueryService", "GetDerived", json!({ "repo": repo(), "type": "diff_class", "key": id_json(&fix_id), "deriveIfMissing": true }))
        .await
        .unwrap();
    assert_eq!(derived["type"], "diff_class");
    assert!(
        derived["valueJson"]
            .as_str()
            .unwrap()
            .contains("src/lib.rs")
    );
    // The ref log records every version with its landed change.
    let rl = t
        .call(
            "RefService",
            "GetRefLog",
            json!({ "repo": repo(), "name": "main" }),
        )
        .await
        .unwrap();
    let versions: Vec<&str> = rl["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions, vec!["1", "2", "3"]);
    assert_eq!(id_text(&rl["entries"][2]["landed"][0]["result"]), fix_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_core_03_land_three_change_stack_then_amend_middle_and_reland() {
    let t = start_default().await;
    let (root, _) = t.tip("main").await;
    let a = id_text(&t.commit(&root, &[("a.txt", b"a1\n")], 10, "A").await["id"]);
    let b = id_text(&t.commit(&a, &[("b.txt", b"b1\n")], 11, "B").await["id"]);
    let c = id_text(&t.commit(&b, &[("c.txt", b"c1\n")], 12, "C").await["id"]);
    let out = t.land(&c).await.expect("land stack");
    assert_eq!(out["landed"].as_array().unwrap().len(), 3);
    assert_eq!(
        out["ref"]["version"], "2",
        "a stack lands as one ref version"
    );
    let rl = t
        .call(
            "RefService",
            "GetRefLog",
            json!({ "repo": repo(), "name": "main", "fromVersion": "2" }),
        )
        .await
        .unwrap();
    assert_eq!(rl["entries"][0]["landed"].as_array().unwrap().len(), 3);
    // Amend the middle change (same ChangeId) and restack C on it.
    let b2 = id_text(&t.commit(&a, &[("b.txt", b"b2\n")], 11, "B amended").await["id"]);
    let c2 = id_text(&t.commit(&b2, &[("c.txt", b"c1\n")], 12, "C").await["id"]);
    let e = t
        .land(&c2)
        .await
        .expect_err("amended ChangeIds already landed");
    assert_eq!(reason(&e), "ALREADY_LANDED");
    // Re-land the amendment as a follow-up change on the new tip.
    let (tip, _) = t.tip("main").await;
    let fix = id_text(
        &t.commit(&tip, &[("b.txt", b"b2\n")], 13, "B amended (follow-up)")
            .await["id"],
    );
    let out = t.land(&fix).await.expect("reland");
    assert_eq!(out["ref"]["version"], "3");
    assert_eq!(t.read("b.txt", "main").await.unwrap(), b"b2\n");
    assert_eq!(t.read("c.txt", "main").await.unwrap(), b"c1\n");
    // Landing the original stack again is an idempotent no-op.
    let again = t.land(&c).await.unwrap();
    assert!(
        again["landed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["skipped"] == true)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_merge_01_rebase_onto_moved_main_resolve_structured_conflict_and_land() {
    let t = start_default().await;
    let (root, _) = t.tip("main").await;
    let base = id_text(
        &t.commit(&root, &[("f.txt", b"one\ntwo\nthree\n")], 20, "base")
            .await["id"],
    );
    t.land(&base).await.unwrap();
    let (t1, _) = t.tip("main").await;
    // Two agents start from the same tip; Z lands first.
    let z = id_text(
        &t.commit(&t1, &[("f.txt", b"one\nZ\nthree\n")], 21, "Z")
            .await["id"],
    );
    let y = id_text(
        &t.commit(&t1, &[("f.txt", b"one\nY\nthree\n")], 22, "Y")
            .await["id"],
    );
    t.land(&z).await.unwrap();
    // Landing Y conflicts; the conflict is data.
    let e = t.land(&y).await.expect_err("conflict");
    assert_eq!(reason(&e), "MERGE_CONFLICT");
    let Some(Payload::Conflicts(set)) = &e.payload else {
        panic!("conflicts payload: {e:?}")
    };
    assert_eq!(set.conflicts.len(), 1);
    assert_eq!(set.conflicts[0].path, b"f.txt");
    assert_eq!(set.conflicts[0].kind, v1::ConflictKind::Content as i32);
    assert_eq!(set.conflicts[0].hunks.len(), 1);
    // Rebase reports the same structured conflict.
    let e = t
        .call(
            "WriteService",
            "Rebase",
            json!({ "repo": repo(), "changeset": id_json(&y), "onto": { "expr": "main" } }),
        )
        .await
        .expect_err("rebase conflict");
    assert_eq!(reason(&e), "MERGE_CONFLICT");
    // Resolve on the moved main, keeping Y's ChangeId, and land.
    let (t2, _) = t.tip("main").await;
    let resolved = id_text(
        &t.commit(&t2, &[("f.txt", b"one\nY+Z\nthree\n")], 22, "Y resolved")
            .await["id"],
    );
    let out = t.land(&resolved).await.expect("land resolution");
    assert_eq!(out["ref"]["version"], "4");
    assert_eq!(t.read("f.txt", "main").await.unwrap(), b"one\nY+Z\nthree\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn policy_denial_lists_missing_requirements() {
    let t = start_default().await;
    let user = t.srv.dev.as_ref().unwrap().user.clone();
    let (root, _) = t.tip("main").await;
    let policy = format!(
        "version = 1\n[members]\nadmin = [\"{user}\"]\nwrite = [\"@humans\", \"@agents\", \"@services\"]\nread = [\"@everyone\"]\n\
         [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n\
         [[land]]\nref = \"main\"\nrequired_checks = [\"test\"]\n"
    );
    let p = id_text(
        &t.commit(
            &root,
            &[(".beehive/policy.toml", policy.as_bytes())],
            30,
            "require checks",
        )
        .await["id"],
    );
    // Landing the policy is governed by the old policy.
    t.land(&p).await.expect("policy lands under the old policy");
    let (tip, _) = t.tip("main").await;
    let c = id_text(&t.commit(&tip, &[("x.txt", b"x\n")], 31, "x").await["id"]);
    let e = t.land(&c).await.expect_err("denied");
    assert_eq!(reason(&e), "POLICY_DENIED");
    let Some(Payload::PolicyDenial(d)) = &e.payload else {
        panic!("{e:?}")
    };
    assert_eq!(d.missing, vec!["check:test".to_string()]);
    assert_eq!(d.rule, "land[main]");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_is_blocked_then_bypassed_with_a_recorded_reason() {
    let t = start_default().await;
    let example = bee_secrets::builtin_rules()
        .into_iter()
        .find(|r| r.id().starts_with("github") && r.confidence().blocks())
        .and_then(|r| r.example().map(str::to_string))
        .expect("a github rule example");
    let (root, _) = t.tip("main").await;
    let e = t
        .commit_as(
            &t.token,
            &root,
            &[("config.yml", example.as_bytes())],
            40,
            "oops",
        )
        .await
        .expect_err("blocked");
    assert_eq!(reason(&e), "SECRET_DETECTED");
    let Some(Payload::PushProtection(block)) = &e.payload else {
        panic!("{e:?}")
    };
    assert_eq!(block.findings[0].path, b"config.yml");
    assert!(block.bypass_allowed);
    assert!(
        !e.message.contains(&example),
        "the secret never appears in errors"
    );
    // Bypass with a reason over gRPC metadata.
    let mut client = v1::write_service_client::WriteServiceClient::new(t.grpc_channel().await);
    let blob = bee_object::blob_id(example.as_bytes());
    let mut req = t.bearer(
        &t.token,
        v1::CreateChangesetRequest {
            repo: prepo(),
            parents: vec![pid(&root)],
            delta: vec![v1::DeltaEntry {
                path: b"config.yml".to_vec(),
                op: Some(v1::delta_entry::Op::Put(v1::Put {
                    mode: 1,
                    blob: Some(pid(&blob.to_text())),
                    size: example.len() as u64,
                })),
            }],
            change_id: pchange(40),
            message: "known test token".into(),
            ..Default::default()
        },
    );
    req.metadata_mut().insert(
        "bee-push-protection-bypass",
        "used_in_tests".parse().unwrap(),
    );
    let resp = client.create_changeset(req).await.expect("bypassed");
    assert!(resp.into_inner().changeset.is_some());
    let audit = t.srv.state.audit.read(1, 1000).await.unwrap();
    let bypass = audit
        .iter()
        .find(|a| a.decision == "bypass")
        .expect("bypass audited");
    assert_eq!(bypass.detail["reason"], "used_in_tests");
    assert!(!bypass.detail.to_string().contains(&example));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn as_auth_01_sub_agent_token_limited_to_docs_with_a_budget_of_five() {
    let t = start_default().await;
    let agent = t
        .call(
            "AuthService",
            "CreateAgent",
            json!({ "display": "doc-writer", "runtime": "claude-code", "purpose": "docs" }),
        )
        .await
        .unwrap();
    let agent_id = agent["id"].as_str().unwrap().to_string();
    let minted = t
        .call(
            "AuthService",
            "MintToken",
            json!({
                "principalId": agent_id,
                "scope": { "writePaths": [B64.encode("/docs")], "budget": { "maxChangesets": "5" } }
            }),
        )
        .await
        .unwrap();
    let token = minted["token"].as_str().unwrap().to_string();
    let who = t
        .call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .unwrap();
    assert_eq!(who["principal"]["id"], agent_id);
    assert_eq!(who["principal"]["kind"], "PRINCIPAL_KIND_AGENT");
    assert_eq!(who["onBehalfOf"][0], t.srv.dev.as_ref().unwrap().user);
    assert_eq!(who["remaining"]["maxChangesets"], "5");
    assert!(who["groups"].as_array().unwrap().contains(&json!("agents")));
    assert!(who["expiresAt"].is_string());
    assert_eq!(who["effectiveScope"]["writePaths"][0], B64.encode("/docs"));
    let access = t
        .call_as(&token, "AuthService", "CheckAccess", json!({ "repo": repo(), "paths": [B64.encode("docs/a.md"), B64.encode("src/a.rs")], "action": "commit" }))
        .await
        .unwrap();
    assert_eq!(access["allowed"], json!([true, false]));
    assert!(
        access["policyEpoch"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
    let (root, _) = t.tip("main").await;
    let mut parent = root.clone();
    for i in 0..5u64 {
        let content = format!("page {i}\n");
        let cs = t
            .commit_as(
                &token,
                &parent,
                &[(&format!("docs/p{i}.md"), content.as_bytes())],
                100 + i,
                "doc",
            )
            .await
            .unwrap_or_else(|e| panic!("commit {i}: {e:?}"));
        parent = id_text(&cs["id"]);
        // Provenance records the delegation chain from the token.
        assert_eq!(cs["meta"]["provenance"]["principalChain"][0], agent_id);
    }
    let e = t
        .commit_as(&token, &parent, &[("docs/p6.md", b"six\n")], 106, "sixth")
        .await
        .expect_err("budget");
    assert_eq!(reason(&e), "BUDGET_EXHAUSTED");
    let e = t
        .commit_as(&token, &parent, &[("src/x.rs", b"no\n")], 107, "outside")
        .await
        .expect_err("scope");
    assert_eq!(reason(&e), "POLICY_DENIED");
    // Offline attenuation by the agent, then revocation cascades to the derived token.
    let narrower = bee_auth::attenuate(
        &token,
        &bee_auth::Attenuation {
            scope: bee_auth::Scope::new().write_paths(["/docs/api"]).unwrap(),
            act_as: None,
        },
    )
    .unwrap();
    t.call_as(&narrower.text, "AuthService", "WhoAmI", json!({}))
        .await
        .expect("derived token works");
    t.call(
        "AuthService",
        "Revoke",
        json!({ "tokenId": minted["tokenId"] }),
    )
    .await
    .expect("revoke");
    let e = t
        .call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .expect_err("revoked");
    assert_eq!(reason(&e), "TOKEN_REVOKED");
    let e = t
        .call_as(&narrower.text, "AuthService", "WhoAmI", json!({}))
        .await
        .expect_err("cascade");
    assert_eq!(reason(&e), "TOKEN_REVOKED");
    // The dev token is unaffected.
    t.call("AuthService", "WhoAmI", json!({})).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redaction_propagates_through_reads_epochs_and_reuploads() {
    let t = start_default().await;
    let (root, _) = t.tip("main").await;
    let cs = t
        .commit(
            &root,
            &[("leak.txt", b"classified\n"), ("ok.txt", b"fine\n")],
            50,
            "leak",
        )
        .await;
    t.land(&id_text(&cs["id"])).await.unwrap();
    let blob = bee_object::blob_id(b"classified\n").to_text();
    let r = t
        .call(
            "AdminService",
            "Redact",
            json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "leaked credentials" }),
        )
        .await
        .unwrap();
    let epoch: u64 = r["redactionEpoch"].as_str().unwrap().parse().unwrap();
    assert!(epoch >= 1);
    let e = t.read("leak.txt", "main").await.expect_err("redacted");
    assert_eq!(reason(&e), "REDACTED");
    assert_eq!(t.read("ok.txt", "main").await.unwrap(), b"fine\n");
    let objs = t
        .call(
            "ObjectService",
            "GetObjects",
            json!({ "repo": repo(), "ids": [id_json(&blob)] }),
        )
        .await
        .unwrap();
    assert_eq!(objs["items"][0]["status"], "ITEM_STATUS_REDACTED");
    // Changeset ids never change.
    let (tip, _) = t.tip("main").await;
    assert_eq!(tip, id_text(&cs["id"]));
    // The epoch is in the gRPC trailers.
    let mut oc = v1::object_service_client::ObjectServiceClient::new(t.grpc_channel().await);
    let resp = oc
        .resolve_ref(t.bearer(
            &t.token,
            v1::ResolveRefRequest {
                repo: prepo(),
                name: "main".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.metadata()
            .get("bee-redaction-epoch")
            .unwrap()
            .to_str()
            .unwrap(),
        epoch.to_string()
    );
    let list = t
        .call("AdminService", "ListRedactions", json!({ "repo": repo() }))
        .await
        .unwrap();
    assert_eq!(
        list["redactions"][0]["state"],
        "REDACTION_STATE_QUARANTINED"
    );
    // Unredact from quarantine.
    t.call(
        "AdminService",
        "Unredact",
        json!({ "repo": repo(), "ids": [id_json(&blob)] }),
    )
    .await
    .unwrap();
    assert_eq!(t.read("leak.txt", "main").await.unwrap(), b"classified\n");
    // Policy epoch trailer: a decimal u64.
    let p: u64 = resp
        .metadata()
        .get("bee-policy-epoch")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(p > 0);
    // Redact again, then purge from quarantine: gone for good, and the bytes cannot come back.
    t.call(
        "AdminService",
        "Redact",
        json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "again" }),
    )
    .await
    .unwrap();
    let purged = t
        .call(
            "AdminService",
            "PurgeNow",
            json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "purge" }),
        )
        .await
        .unwrap();
    assert_eq!(id_text(&purged["purged"][0]), blob);
    let other = bee_object::blob_id(b"fine\n").to_text();
    let e = t
        .call(
            "AdminService",
            "PurgeNow",
            json!({ "repo": repo(), "ids": [id_json(&other)] }),
        )
        .await
        .expect_err("not quarantined");
    assert_eq!(reason(&e), "NOT_QUARANTINED");
    let e = t.read("leak.txt", "main").await.expect_err("purged");
    assert_eq!(reason(&e), "REDACTED");
    let e = t
        .call("WriteService", "UploadObjects", json!({ "messages": [{ "repo": repo(), "id": id_json(&blob), "offset": "0", "data": B64.encode(b"classified\n"), "last": true }] }))
        .await
        .expect_err("blocklisted");
    assert_eq!(reason(&e), "REDACTED");
    let e = t
        .call(
            "AdminService",
            "Unredact",
            json!({ "repo": repo(), "ids": [id_json(&blob)] }),
        )
        .await
        .expect_err("purged");
    assert_eq!(reason(&e), "NOT_QUARANTINED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn import_a_small_git_repo_then_log_it() {
    let t = start_default().await;
    let src = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(src.path())
            .env("GIT_AUTHOR_NAME", "Ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    for i in 0..3 {
        std::fs::write(src.path().join("README.md"), format!("version {i}\n")).unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", &format!("commit {i}")]);
    }
    let head_sha = git(&["rev-parse", "HEAD"]);
    let started = t
        .call(
            "ImportService",
            "StartImport",
            json!({ "source": "dev/imported", "sourceKind": "IMPORT_SOURCE_KIND_GIT", "sourceUrl": src.path().display().to_string() }),
        )
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    let mut state = String::new();
    for _ in 0..200 {
        let g = t
            .call("ImportService", "GetImport", json!({ "id": id }))
            .await
            .unwrap();
        state = g["state"].as_str().unwrap_or("").to_string();
        if state == "IMPORT_STATE_LIVE" || state == "IMPORT_STATE_FAILED" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(state, "IMPORT_STATE_LIVE");
    let imported = json!({ "fullName": "dev/imported" });
    let log = t
        .call(
            "QueryService",
            "Log",
            json!({ "repo": imported, "from": { "expr": "main" }, "includeMeta": true }),
        )
        .await
        .unwrap();
    let msgs: Vec<&str> = log["changesets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["meta"]["message"].as_str().unwrap().trim_end())
        .collect();
    assert_eq!(msgs, vec!["commit 2", "commit 1", "commit 0"]);
    // The git id resolves to the same changeset (exact-id round trip).
    let r = t
        .call(
            "QueryService",
            "Resolve",
            json!({ "repo": imported, "revisions": [{ "expr": format!("git:{head_sha}") }] }),
        )
        .await
        .unwrap();
    assert_eq!(id_text(&r["ids"][0]), id_text(&log["changesets"][0]["id"]));
    let report = t
        .call("ImportService", "GetFidelityReport", json!({ "id": id }))
        .await
        .unwrap();
    let commits = report["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["category"] == "commits")
        .unwrap();
    assert_eq!(commits["imported"], "3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_forms_grpc_json_and_mcp_return_identical_data() {
    let t = start_default().await;
    let (root, _) = t.tip("main").await;
    let cs = t
        .commit(&root, &[("hello.txt", b"hi\n")], 60, "hello")
        .await;
    t.land(&id_text(&cs["id"])).await.unwrap();
    let request = json!({ "repo": repo(), "from": { "expr": "main" }, "includeMeta": true });
    // gRPC.
    let mut qc = v1::query_service_client::QueryServiceClient::new(t.grpc_channel().await);
    let grpc = qc
        .log(t.bearer(
            &t.token,
            v1::LogRequest {
                repo: prepo(),
                from: Some(v1::Revision {
                    expr: "main".into(),
                }),
                include_meta: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let grpc_json = serde_json::to_value(&grpc).unwrap();
    // HTTP/JSON.
    let json_form = t
        .call("QueryService", "Log", request.clone())
        .await
        .unwrap();
    assert_eq!(grpc_json, json_form);
    // MCP over streamable HTTP at /mcp.
    let mcp = format!("{}/mcp", t.url());
    let auth = format!("Bearer {}", t.token);
    let headers = [
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
        ("authorization", auth.as_str()),
    ];
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } } });
    let (status, h, _) = http("POST", &mcp, &headers, serde_json::to_vec(&init).unwrap()).await;
    assert_eq!(status, 200);
    let session = h
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let mut with_session = headers.to_vec();
    with_session.push(("mcp-session-id", session.as_str()));
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    http(
        "POST",
        &mcp,
        &with_session,
        serde_json::to_vec(&note).unwrap(),
    )
    .await;
    let call = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "query_log", "arguments": request } });
    let (status, _, body) = http(
        "POST",
        &mcp,
        &with_session,
        serde_json::to_vec(&call).unwrap(),
    )
    .await;
    assert_eq!(status, 200);
    let text = String::from_utf8(body).unwrap();
    let doc: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            let data = text
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .next_back()
                .unwrap()
                .trim()
                .to_string();
            serde_json::from_str(&data).unwrap()
        }
    };
    let structured = &doc["result"]["structuredContent"];
    assert_eq!(structured, &json_form, "mcp result: {doc}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_urls_serve_verified_bytes_for_large_objects() {
    let t = start_default().await;
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let (root, _) = t.tip("main").await;
    let cs = t.commit(&root, &[("big.bin", &big)], 70, "big").await;
    t.land(&id_text(&cs["id"])).await.unwrap();
    let id = bee_object::blob_id(&big).to_text();
    let r = t
        .call(
            "ObjectService",
            "GetObjects",
            json!({ "repo": repo(), "ids": [id_json(&id)], "allowSignedUrls": true }),
        )
        .await
        .unwrap();
    let url = r["items"][0]["signedUrl"]
        .as_str()
        .expect("signed url")
        .to_string();
    assert!(url.starts_with(t.url()));
    let (status, _, body) = http("GET", &url, &[], Vec::new()).await;
    assert_eq!(status, 200);
    assert!(bee_store_blob::bytes_match_id(
        &bee_core::ObjectId::parse(&id).unwrap(),
        &body
    ));
    // A tampered signature is refused.
    let (status, _, _) = http("GET", &url.replace("sig=", "sig=00"), &[], Vec::new()).await;
    assert_eq!(status, 401);
    // Without allow_signed_urls the bytes stream inline, verified by id.
    let r = t
        .call(
            "ObjectService",
            "GetObjects",
            json!({ "repo": repo(), "ids": [id_json(&id)] }),
        )
        .await
        .unwrap();
    let mut bytes = Vec::new();
    for item in r["items"].as_array().unwrap() {
        bytes.extend(B64.decode(item["data"].as_str().unwrap()).unwrap());
    }
    assert_eq!(bytes, big);
    // HasObjects cannot be used to probe objects the caller cannot read.
    let unknown = bee_object::blob_id(b"never uploaded").to_text();
    let h = t
        .call(
            "ObjectService",
            "HasObjects",
            json!({ "repo": repo(), "ids": [id_json(&id), id_json(&unknown)] }),
        )
        .await
        .unwrap();
    assert_eq!(h["present"], json!([true, false]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refs_follow_fast_forward_rules_and_protected_refs_move_only_by_land() {
    let t = start_default().await;
    let user = t.srv.dev.as_ref().unwrap().user.clone();
    let (root, _) = t.tip("main").await;
    let a = id_text(&t.commit(&root, &[("a", b"1\n")], 80, "a").await["id"]);
    let b = id_text(&t.commit(&a, &[("a", b"2\n")], 81, "b").await["id"]);
    let other = id_text(&t.commit(&root, &[("z", b"z\n")], 82, "z").await["id"]);
    let ws = format!("ws/{user}/feature");
    let r = t
        .call(
            "RefService",
            "CreateRef",
            json!({ "repo": repo(), "name": ws, "target": id_json(&a), "reason": "start" }),
        )
        .await
        .unwrap();
    assert_eq!(r["version"], "1");
    let r = t
        .call(
            "RefService",
            "UpdateRef",
            json!({ "repo": repo(), "name": ws, "target": id_json(&b), "expectedVersion": "1" }),
        )
        .await
        .unwrap();
    assert_eq!(r["version"], "2");
    let e = t.call("RefService", "UpdateRef", json!({ "repo": repo(), "name": ws, "target": id_json(&other), "expectedVersion": "2" })).await.expect_err("nff");
    assert_eq!(reason(&e), "NOT_FAST_FORWARD");
    let e = t
        .call(
            "RefService",
            "UpdateRef",
            json!({ "repo": repo(), "name": ws, "target": id_json(&b), "expectedVersion": "1" }),
        )
        .await
        .expect_err("stale");
    assert_eq!(reason(&e), "VERSION_MISMATCH");
    let r = t
        .call("RefService", "UpdateRef", json!({ "repo": repo(), "name": ws, "target": id_json(&other), "expectedVersion": "2", "allowNonFastForward": true }))
        .await
        .unwrap();
    assert_eq!(r["version"], "3");
    let e = t
        .call(
            "RefService",
            "UpdateRef",
            json!({ "repo": repo(), "name": "main", "target": id_json(&a) }),
        )
        .await
        .expect_err("protected");
    assert_eq!(reason(&e), "POLICY_DENIED");
    let e = t
        .call(
            "RefService",
            "CreateRef",
            json!({ "repo": repo(), "name": "bad..name", "target": id_json(&a) }),
        )
        .await
        .expect_err("grammar");
    assert_eq!(e.subcode, "invalid_ref_name");
    t.call(
        "RefService",
        "DeleteRef",
        json!({ "repo": repo(), "name": ws }),
    )
    .await
    .unwrap();
    // Recreating continues the numbering.
    let r = t
        .call(
            "RefService",
            "CreateRef",
            json!({ "repo": repo(), "name": ws, "target": id_json(&a) }),
        )
        .await
        .unwrap();
    assert_eq!(r["version"], "5");
    // Land onto a workspace ref is a plain CAS land.
    let out = t
        .call(
            "WriteService",
            "Land",
            json!({ "repo": repo(), "ref": ws, "head": id_json(&b) }),
        )
        .await
        .unwrap();
    assert_eq!(out["ref"]["version"], "6");
    let r = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": repo(), "name": ws, "atVersion": "2" }),
        )
        .await
        .unwrap();
    assert_eq!(id_text(&r["ref"]["target"]), b);
    // Read-your-writes: min_version at the produced version answers immediately.
    let r = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": repo(), "name": ws, "minVersion": "6" }),
        )
        .await
        .unwrap();
    assert_eq!(r["ref"]["version"], "6");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_services_answer_unimplemented_on_both_surfaces() {
    use tower::ServiceExt;
    let t = start_default().await;
    let e = t
        .call("NoSuchService", "Nope", json!({}))
        .await
        .expect_err("unimplemented");
    assert_eq!(reason(&e), "UNIMPLEMENTED");
    // gRPC: an empty length-prefixed message to a path no service owns.
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("{}/bee.v1.NoSuchService/Nope", t.url()))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("authorization", format!("Bearer {}", t.token))
        .body(tonic::body::Body::new(http_body_util::Full::new(
            bytes::Bytes::from_static(&[0, 0, 0, 0, 0]),
        )))
        .unwrap();
    let resp = t
        .grpc_channel()
        .await
        .oneshot(req)
        .await
        .expect("grpc call");
    let status = tonic::Status::from_header_map(resp.headers())
        .map(|s| s.code())
        .unwrap_or(tonic::Code::Ok);
    assert_eq!(status, tonic::Code::Unimplemented);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unauthenticated_and_throttled_requests_get_contract_errors() {
    let t = start_with(|c| {
        c.admission.per_principal_rps = 2;
    })
    .await;
    let mut c = bee_mcp::ClientConfig::new(t.url().to_string());
    c.token = None;
    let e = bee_mcp::RpcClient::new(&c)
        .call_rpc_json("AuthService", "WhoAmI", &json!({}))
        .await
        .expect_err("anon");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
    let mut throttled = None;
    for _ in 0..20 {
        if let Err(e) = t.call("AuthService", "WhoAmI", json!({})).await {
            throttled = Some(e);
            break;
        }
    }
    let e = throttled.expect("throttled");
    assert_eq!(reason(&e), "RESOURCE_EXHAUSTED");
    assert!(e.retryable);
    assert!(e.retry_after_ms > 0);
    // The HTTP response carries retry-after as well.
    let auth = format!("Bearer {}", t.token);
    let mut saw = false;
    for _ in 0..10 {
        let (status, h, _) = http(
            "POST",
            &format!("{}/bee.v1.AuthService/WhoAmI", t.url()),
            &[
                ("content-type", "application/json"),
                ("authorization", auth.as_str()),
            ],
            b"{}".to_vec(),
        )
        .await;
        if status == 429 {
            assert!(h.contains_key("retry-after") || h.contains_key("trailer-retry-after"));
            saw = true;
            break;
        }
    }
    assert!(saw);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_runs_fsck_and_gc_dry_run_then_real() {
    let t = start_default().await;
    let (root, _) = t.tip("main").await;
    let cs = t.commit(&root, &[("k.txt", b"keep\n")], 90, "keep").await;
    t.land(&id_text(&cs["id"])).await.unwrap();
    let f = t
        .call("AdminService", "RunFsck", json!({ "org": "dev" }))
        .await
        .unwrap();
    assert_eq!(f["clean"], true, "{f}");
    assert!(
        f["changesetsChecked"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 2
    );
    let dry = t
        .call(
            "AdminService",
            "RunGc",
            json!({ "org": "dev", "dryRun": true }),
        )
        .await
        .unwrap();
    assert_eq!(dry["dryRun"], true);
    let real = t
        .call("AdminService", "RunGc", json!({ "org": "dev" }))
        .await
        .unwrap();
    assert!(real["dryRun"].is_null());
    assert_eq!(t.read("k.txt", "main").await.unwrap(), b"keep\n");
    let audit = t
        .call(
            "AuditService",
            "QueryAuditLog",
            json!({ "org": "dev", "oldestFirst": true,
                    "filter": { "actionPrefixes": ["admin."] } }),
        )
        .await
        .expect("store operations are visible through the forge audit log");
    let entries = audit["entries"].as_array().expect("audit entries");
    assert!(
        entries.iter().any(|e| e["action"] == "admin.fsck"),
        "{audit}"
    );
    assert_eq!(
        entries.iter().filter(|e| e["action"] == "admin.gc").count(),
        2,
        "{audit}"
    );
    assert!(
        entries.iter().all(|e| e["actor"]["id"] == t.dev_user()),
        "{audit}"
    );
    assert!(
        entries
            .iter()
            .any(|e| e["after"].to_string().contains("dry_run")),
        "{audit}"
    );
    // Metrics expose fixed names.
    let (status, _, body) = http("GET", &format!("{}/metrics", t.url()), &[], Vec::new()).await;
    assert_eq!(status, 200);
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("bee_rpc_requests_total{method=\"/bee.v1.WriteService/Land\""));
    assert!(text.contains("bee_land_batches_total"));
    let (status, _, _) = http("GET", &format!("{}/readyz", t.url()), &[], Vec::new()).await;
    assert_eq!(status, 200);
    // Every state change wrote a verifiable audit entry.
    let entries = t.srv.state.audit.read(1, 10_000).await.unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.action == "/bee.v1.WriteService/Land")
    );
    assert!(
        entries
            .iter()
            .any(|e| e.action == "/bee.v1.WriteService/CreateChangeset")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_changeset_checks_expected_root_and_missing_objects() {
    let t = start_default().await;
    let (root, _) = t.tip("main").await;
    let ids = t.upload(&[b"content\n"]).await;
    let base = json!({
        "repo": repo(),
        "parents": [id_json(&root)],
        "delta": [{ "path": B64.encode("f"), "put": { "mode": "MODE_FILE", "blob": id_json(&ids[0]), "size": "8" } }],
        "changeId": change_id(500),
        "message": "m",
        "authorTime": { "time": "2026-10-18T00:00:00Z" },
    });
    let mut wrong = base.clone();
    wrong["expectedRootTree"] = id_json(&bee_object::Tree::empty().id().to_text());
    let e = t
        .call("WriteService", "CreateChangeset", wrong)
        .await
        .expect_err("mismatch");
    assert_eq!(reason(&e), "ROOT_TREE_MISMATCH");
    let first = t
        .call("WriteService", "CreateChangeset", base.clone())
        .await
        .unwrap();
    // Identical request: same id, no error.
    let second = t
        .call("WriteService", "CreateChangeset", base.clone())
        .await
        .unwrap();
    assert_eq!(first["changeset"]["id"], second["changeset"]["id"]);
    let missing = bee_object::blob_id(b"nobody uploaded this").to_text();
    let mut m = base.clone();
    m["delta"][0]["put"]["blob"] = id_json(&missing);
    let e = t
        .call("WriteService", "CreateChangeset", m)
        .await
        .expect_err("missing");
    assert_eq!(reason(&e), "MISSING_OBJECT");
    let mut bad = base.clone();
    bad["delta"][0]["path"] = json!(B64.encode("a//b"));
    let e = t
        .call("WriteService", "CreateChangeset", bad)
        .await
        .expect_err("path");
    assert_eq!(e.subcode, "invalid_path");
    // A merge may be in a stack (SPEC §7.4); the single-change Rebase still refuses it.
    let x = id_text(&t.commit(&root, &[("x", b"x\n")], 501, "x").await["id"]);
    let y = id_text(&t.commit(&root, &[("y", b"y\n")], 502, "y").await["id"]);
    let merged = t
        .call("WriteService", "Merge", json!({ "repo": repo(), "ours": { "expr": x }, "theirs": { "expr": y }, "changeId": change_id(503) }))
        .await
        .unwrap();
    t.land(&id_text(&merged["changeset"]["id"]))
        .await
        .expect("a stack with a merge lands");
    let e = t
        .call("WriteService", "Rebase", json!({ "repo": repo(), "changeset": merged["changeset"]["id"], "onto": { "expr": "main" } }))
        .await
        .expect_err("multi parent");
    assert_eq!(reason(&e), "MULTI_PARENT");
}
