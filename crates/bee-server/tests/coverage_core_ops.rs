//! Coverage for PRODUCT §4.1, §4.10 and §4.22 rows through an in-process dev server: the
//! permissions behind redaction, purge and ref management; the read gate of the query RPCs;
//! redacted entries carried forward; the consumer rule after an unredaction; content-bearing
//! derived records and purge; land attempt limits and states; rate limits and bulk costs;
//! integrity alerts; failure classes; field compatibility; tracing; the production checklist.

mod common;

use std::sync::Arc;

use base64::Engine as _;
use bee_server::server::{ServerOptions, start};
use bee_store::{Fault, FaultInjector, FaultRule};
use common::*;
use serde_json::{Value, json};

async fn collaborate(t: &TestServer, id: &str, token: &str, role: &str) {
    let r = t
        .call(
            "RepoService",
            "AddCollaborator",
            json!({ "repo": repo(), "principalId": id, "role": role }),
        )
        .await
        .expect("add collaborator");
    if let Some(inv) = r["invitation"]["id"].as_str() {
        t.call_as(
            token,
            "RepoService",
            "AcceptRepoInvitation",
            json!({ "invitationId": inv }),
        )
        .await
        .expect("accept invitation");
    }
}

/// Lands one file and returns (tip, blob id of `content`).
async fn land_file(t: &TestServer, path: &str, content: &[u8], cid: u64) -> (String, String) {
    let (tip, _) = t.tip("main").await;
    let c = t.commit(&tip, &[(path, content)], cid, "file").await;
    let cs = id_text(&c["id"]);
    t.land(&cs).await.expect("land");
    (cs, bee_object::blob_id(content).to_text())
}

fn err_reason(r: Result<Value, bee_proto::v1::ErrorDetail>) -> String {
    match r {
        Ok(_) => "OK".into(),
        Err(e) => e.reason,
    }
}

/// F410-003: `redact`, `purge`, `manage_refs` and `force_update` are separate permissions.
/// Redact needs `redact`; purging (`PurgeNow`, or `Redact` with `purge_now`) also needs
/// `purge`; creating and moving plain refs needs `manage_refs`; a non-fast-forward move needs
/// `force_update`. Both halves decide: the token's actions and the caller's role.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redact_purge_manage_refs_and_force_update_are_separate_permissions() {
    let t = start_default().await;
    // Roles: a maintainer manages refs but neither redacts, purges nor forces.
    let (maint_id, maint) = t.principal("human", "Mai Maintainer", "").await;
    collaborate(&t, &maint_id, &maint, "REPO_ROLE_MAINTAIN").await;
    let (writer_id, writer) = t.principal("human", "Wes Writer", "").await;
    collaborate(&t, &writer_id, &writer, "REPO_ROLE_WRITE").await;
    let user = t.dev_user();
    let policy = format!(
        "version = 1\n[members]\nadmin = [\"{user}\"]\nmaintain = [\"{maint_id}\"]\n\
         write = [\"@humans\", \"@agents\"]\nread = [\"@everyone\"]\n\
         [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n"
    );
    let (root, _) = t.tip("main").await;
    let p = t
        .commit(
            &root,
            &[(".beehive/policy.toml", policy.as_bytes())],
            9,
            "policy",
        )
        .await;
    t.land(&id_text(&p["id"])).await.expect("policy");
    let (base, blob) = land_file(&t, "leak.txt", b"api key\n", 1).await;
    let (tip, _) = land_file(&t, "next.txt", b"next\n", 2).await;

    // A token attenuated to read + redact: redaction works, purging does not.
    let mint = t
        .call(
            "AuthService",
            "MintToken",
            json!({ "scope": { "actions": ["read", "redact"] } }),
        )
        .await
        .expect("mint");
    let redactor = mint["token"].as_str().unwrap().to_string();
    let redact = |tok: String, purge_now: bool| {
        let t = &t;
        let blob = blob.clone();
        async move {
            t.call_as(
                &tok,
                "AdminService",
                "Redact",
                json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "leak", "purgeNow": purge_now }),
            )
            .await
        }
    };
    let e = redact(redactor.clone(), true).await.expect_err("no purge");
    assert_eq!(e.reason, "POLICY_DENIED", "{e:?}");
    assert!(e.message.contains("purge") || format!("{e:?}").contains("token:purge"));
    redact(redactor.clone(), false).await.expect("redact");
    let purge = |tok: String| {
        let t = &t;
        let blob = blob.clone();
        async move {
            t.call_as(
                &tok,
                "AdminService",
                "PurgeNow",
                json!({ "repo": repo(), "ids": [id_json(&blob)] }),
            )
            .await
        }
    };
    let e = purge(redactor.clone()).await.expect_err("no purge");
    assert_eq!(e.reason, "POLICY_DENIED", "{e:?}");
    // Unredact is part of `redact`.
    t.call_as(
        &redactor,
        "AdminService",
        "Unredact",
        json!({ "repo": repo(), "ids": [id_json(&blob)] }),
    )
    .await
    .expect("unredact with redact");

    assert_eq!(
        err_reason(redact(maint.clone(), false).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(err_reason(purge(maint.clone()).await), "PERMISSION_DENIED");
    let update = |tok: String, name: &'static str, target: String, expected: Option<u64>| {
        let t = &t;
        async move {
            let req = json!({ "repo": repo(), "name": name, "target": id_json(&target) });
            match expected {
                Some(v) => {
                    let mut req = req;
                    req["expectedVersion"] = json!(v.to_string());
                    req["allowNonFastForward"] = json!(true);
                    t.call_as(&tok, "RefService", "UpdateRef", req).await
                }
                None => t.call_as(&tok, "RefService", "CreateRef", req).await,
            }
        }
    };
    // A writer may not create a plain ref; a maintainer may.
    let w = update(writer.clone(), "release-notes", tip.clone(), None).await;
    assert_eq!(err_reason(w.clone()), "PERMISSION_DENIED", "{w:?}");
    let r = update(maint.clone(), "release-notes", tip.clone(), None)
        .await
        .expect("maintainer creates a ref");
    let v: u64 = r["version"].as_str().unwrap().parse().unwrap();
    // Moving it backwards is a force update: not for a maintainer, fine for an admin.
    let e = update(maint.clone(), "release-notes", base.clone(), Some(v))
        .await
        .expect_err("a maintainer may not force");
    assert_eq!(e.reason, "POLICY_DENIED", "{e:?}");
    assert!(e.message.contains("force_update"), "{e:?}");
    update(t.token.clone(), "release-notes", base.clone(), Some(v))
        .await
        .expect("admin forces");
    // The admin holds redact and purge.
    redact(t.token.clone(), false).await.expect("admin redacts");
    purge(t.token.clone()).await.expect("admin purges");
}

/// F410-004: Rebase, IsAncestor, MergeBase, GetDerived and Resolve need read access to the
/// repo (a caller who cannot read a private repo is told it does not exist); Rebase also
/// needs `commit`. ListRedactions needs no permission beyond
/// seeing the repo: on a public repo any valid token lists redactions, without `redact`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_rpcs_need_read_and_list_redactions_needs_only_a_valid_token() {
    let t = start_default().await;
    let user = t.dev_user();
    t.land_policy(
        &format!("[[access]]\npath = \"/secret\"\nread = [\"{user}\"]\nwrite = [\"{user}\"]\n"),
        1,
    )
    .await;
    let (a, blob) = land_file(&t, "secret/key", b"hunter2\n", 2).await;
    let (b, _) = land_file(&t, "open.txt", b"open\n", 3).await;
    let side = t.commit(&a, &[("side.txt", b"side\n")], 4, "side").await;
    let side = id_text(&side["id"]);
    t.call(
        "AdminService",
        "Redact",
        json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "leak" }),
    )
    .await
    .expect("redact");
    let calls = |tok: String| {
        let t = &t;
        let (a, b, side) = (a.clone(), b.clone(), side.clone());
        async move {
            let mut out = Vec::new();
            for (method, req) in [
                (
                    "Rebase",
                    json!({ "repo": repo(), "changeset": id_json(&side), "onto": { "expr": "main" } }),
                ),
                (
                    "IsAncestor",
                    json!({ "repo": repo(), "ancestor": { "expr": a }, "descendant": { "expr": b } }),
                ),
                (
                    "MergeBase",
                    json!({ "repo": repo(), "a": { "expr": side }, "b": { "expr": b } }),
                ),
                (
                    "GetDerived",
                    json!({ "repo": repo(), "type": "graph", "key": id_json(&b), "deriveIfMissing": true }),
                ),
                (
                    "Resolve",
                    json!({ "repo": repo(), "revisions": [{ "expr": "main" }] }),
                ),
            ] {
                let service = if method == "Rebase" {
                    "WriteService"
                } else {
                    "QueryService"
                };
                out.push((
                    method,
                    err_reason(t.call_as(&tok, service, method, req).await),
                ));
            }
            out
        }
    };
    // The owner reads everything.
    for (m, r) in calls(t.token.clone()).await {
        assert_eq!(r, "OK", "{m} for the owner");
    }
    // A stranger to the private repo: every one of them is NOT_FOUND.
    let (_, stranger) = t.principal("human", "Sam Stranger", "").await;
    for (m, r) in calls(stranger.clone()).await {
        assert_eq!(r, "NOT_FOUND", "{m} for a stranger");
    }
    let e = t
        .call_as(
            &stranger,
            "AdminService",
            "ListRedactions",
            json!({ "repo": repo() }),
        )
        .await
        .expect_err("private repo");
    assert_eq!(e.reason, "NOT_FOUND");
    // A reader who cannot read `/secret`: the reads work.
    let (reader_id, reader) = t.principal("human", "Rita Reader", "").await;
    collaborate(&t, &reader_id, &reader, "REPO_ROLE_READ").await;
    for (m, r) in calls(reader.clone()).await {
        if m == "Rebase" {
            // Rebase stores a changeset: it needs `commit` as well as read.
            assert_eq!(r, "PERMISSION_DENIED", "Rebase for a reader");
        } else {
            assert_eq!(r, "OK", "{m} for a reader");
        }
    }
    // ListRedactions: the reader (no `redact`, cannot read the redacted path) lists it.
    let list = t
        .call_as(
            &reader,
            "AdminService",
            "ListRedactions",
            json!({ "repo": repo() }),
        )
        .await
        .expect("reader lists");
    assert_eq!(id_text(&list["redactions"][0]["id"]), blob);
    // Public: any valid token, member or not, lists redactions.
    t.call(
        "RepoService",
        "UpdateRepo",
        json!({ "repo": repo(), "visibility": "VISIBILITY_PUBLIC" }),
    )
    .await
    .expect("public");
    let list = t
        .call_as(
            &stranger,
            "AdminService",
            "ListRedactions",
            json!({ "repo": repo() }),
        )
        .await
        .expect("any valid token lists a public repo's redactions");
    assert_eq!(id_text(&list["redactions"][0]["id"]), blob);
    // Without a valid token there is no answer.
    let e = t
        .call_as(
            "not-a-token",
            "AdminService",
            "ListRedactions",
            json!({ "repo": repo() }),
        )
        .await
        .expect_err("invalid token");
    assert_eq!(e.reason, "UNAUTHENTICATED");
}

/// F410-010: a redacted entry that a later changeset carries forward unchanged stays valid.
/// Commits and lands on top of a tree holding a redacted blob succeed, the entry keeps its id,
/// reads of it answer REDACTED, and fsck stays clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redacted_entries_carried_forward_stay_valid() {
    let t = start_default().await;
    let (root, _) = t.tip("main").await;
    let first = t
        .commit(
            &root,
            &[("leak.txt", b"password=1\n"), ("app.txt", b"v1\n")],
            1,
            "both",
        )
        .await;
    let first = id_text(&first["id"]);
    t.land(&first).await.expect("land");
    let blob = bee_object::blob_id(b"password=1\n").to_text();
    t.call(
        "AdminService",
        "Redact",
        json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "leak" }),
    )
    .await
    .expect("redact");
    // Change only `app.txt`: the redacted entry rides along.
    let next = t.commit(&first, &[("app.txt", b"v2\n")], 2, "app").await;
    let next = id_text(&next["id"]);
    t.land(&next).await.expect("land over a redacted entry");
    // A stack landed by rebase over it, from an older parent.
    let other = t.commit(&first, &[("other.txt", b"o\n")], 3, "other").await;
    t.land(&id_text(&other["id"])).await.expect("rebased land");
    let listed = t
        .call(
            "QueryService",
            "ListTree",
            json!({ "repo": repo(), "at": { "expr": "main" }, "path": "" }),
        )
        .await
        .expect("list");
    let entry = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == B64.encode("leak.txt"))
        .cloned()
        .unwrap_or_else(|| panic!("still listed: {listed}"));
    assert_eq!(id_text(&entry["id"]), blob, "same id");
    assert_eq!(
        reason(&t.read("leak.txt", "main").await.unwrap_err()),
        "REDACTED"
    );
    assert_eq!(t.read("app.txt", "main").await.unwrap(), b"v2\n");
    let fsck = t
        .call("AdminService", "RunFsck", json!({ "repo": repo() }))
        .await
        .expect("fsck");
    assert!(
        fsck["violations"].as_array().is_none_or(|v| v.is_empty()),
        "{fsck}"
    );
    // Putting the redacted id again (a new entry) is refused: only carrying forward is valid.
    let e = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": repo(),
                "parents": [id_json(&next)],
                "delta": [{ "path": B64.encode("copy.txt"), "put": { "mode": "MODE_FILE", "blob": id_json(&blob), "size": "11" } }],
                "changeId": change_id(9),
                "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": "copy",
            }),
        )
        .await
        .expect_err("redacted put");
    assert_eq!(e.reason, "MISSING_OBJECT", "{e:?}");
}

/// F410-008: consumers keep a redaction set by listing with `since_epoch` = the epoch they last
/// saw. An unredaction bumps the epoch and appears as an UNREDACTED record, so the incremental
/// listing removes the id; listing from epoch 0 gives the same set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unredaction_reaches_incremental_consumers() {
    let t = start_default().await;
    let (_, a) = land_file(&t, "a.txt", b"aaa\n", 1).await;
    let (_, b) = land_file(&t, "b.txt", b"bbb\n", 2).await;
    for id in [&a, &b] {
        t.call(
            "AdminService",
            "Redact",
            json!({ "repo": repo(), "ids": [id_json(id)], "reason": "r" }),
        )
        .await
        .expect("redact");
    }
    let list = |since: u64| {
        let t = &t;
        async move {
            t.call(
                "AdminService",
                "ListRedactions",
                json!({ "repo": repo(), "sinceEpoch": since.to_string() }),
            )
            .await
            .expect("list")
        }
    };
    // The consumer's state after a full listing.
    let full = list(0).await;
    let mut set = std::collections::BTreeSet::new();
    let mut seen = 0u64;
    let apply = |set: &mut std::collections::BTreeSet<String>, seen: &mut u64, l: &Value| {
        for r in l["redactions"].as_array().cloned().unwrap_or_default() {
            let id = id_text(&r["id"]);
            *seen = (*seen).max(r["epoch"].as_str().unwrap().parse().unwrap());
            if r["state"] == "REDACTION_STATE_UNREDACTED" {
                set.remove(&id);
            } else {
                set.insert(id);
            }
        }
    };
    apply(&mut set, &mut seen, &full);
    assert_eq!(set.len(), 2);
    let r = t
        .call(
            "AdminService",
            "Unredact",
            json!({ "repo": repo(), "ids": [id_json(&a)] }),
        )
        .await
        .expect("unredact");
    let epoch: u64 = r["redactionEpoch"].as_str().unwrap().parse().unwrap();
    assert!(epoch > seen, "an unredaction bumps the epoch");
    let delta = list(seen + 1).await;
    let records = delta["redactions"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{delta}");
    assert_eq!(records[0]["state"], "REDACTION_STATE_UNREDACTED");
    apply(&mut set, &mut seen, &delta);
    assert_eq!(set, [b.clone()].into_iter().collect());
    // Re-listing from epoch 0 agrees with the incremental state.
    let mut fresh = std::collections::BTreeSet::new();
    let mut s0 = 0;
    apply(&mut fresh, &mut s0, &list(0).await);
    assert_eq!(fresh, set);
    assert_eq!(t.read("a.txt", "main").await.unwrap(), b"aaa\n");
}

async fn metric(t: &TestServer, line_prefix: &str) -> f64 {
    let (status, _, body) = http("GET", &format!("{}/metrics", t.url()), &[], Vec::new()).await;
    assert_eq!(status, 200);
    String::from_utf8(body)
        .unwrap()
        .lines()
        .find(|l| l.starts_with(line_prefix))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// F41-017 and F41-018: a land makes at most `land.attempt_budget` ref CAS attempts, then fails
/// with the retryable `LAND_CONTENTION` (`UNAVAILABLE`); a land that wins on a later attempt
/// reports how many it took. Each land ends in one named result state (counted in
/// `bee_lands_total{result}`): `landed`, `skipped`, `conflict`, `denied`, `contention`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn land_attempts_are_bounded_and_running_out_is_land_contention() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    cfg.land.attempt_budget = 3;
    let faults = FaultInjector::new();
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = true;
    opts.faults = Some(faults.clone());
    let srv = start(opts).await.expect("server");
    let token = srv.dev.as_ref().unwrap().token.clone();
    let t = TestServer {
        srv,
        token,
        dir,
        cfg,
    };
    let lost = || {
        Fault::Error(bee_core::BeeError::new(
            bee_core::Reason::VersionMismatch,
            "another writer moved the ref",
        ))
    };
    let (root, v0) = t.tip("main").await;

    // Every CAS loses: exactly `attempt_budget` attempts, then LAND_CONTENTION.
    let c = t.commit(&root, &[("a.txt", b"a\n")], 1, "a").await;
    let a = id_text(&c["id"]);
    faults.add(FaultRule::always("meta.update_ref", lost()));
    let before = faults.hits_of("meta.update_ref");
    let e = t.land(&a).await.expect_err("contention");
    assert_eq!(e.reason, "LAND_CONTENTION", "{e:?}");
    assert!(e.retryable);
    // Over gRPC the status code is UNAVAILABLE.
    let mut wc =
        bee_proto::v1::write_service_client::WriteServiceClient::new(t.grpc_channel().await);
    let st = wc
        .land(t.bearer(
            &t.token,
            bee_proto::v1::LandRequest {
                repo: prepo(),
                r#ref: "main".into(),
                head: Some(pid(&a)),
                ..Default::default()
            },
        ))
        .await
        .expect_err("contention over gRPC");
    assert_eq!(st.code(), tonic::Code::Unavailable);
    assert_eq!(detail(&st).reason, "LAND_CONTENTION");
    assert_eq!(
        faults.hits_of("meta.update_ref") - before,
        6,
        "the budget, twice"
    );
    faults.clear_rules();
    assert_eq!(t.tip("main").await.1, v0, "nothing published");
    assert_eq!(
        metric(&t, "bee_lands_total{result=\"contention\"}").await,
        2.0
    );

    // Two lost CASes, then a win: one land, attempts = 3.
    faults.add(FaultRule {
        point: "meta.update_ref".into(),
        fault: lost(),
        skip: 0,
        times: Some(2),
    });
    let r = t.land(&a).await.expect("third attempt wins");
    assert_eq!(r["attempts"], 3, "{r}");
    faults.clear_rules();
    assert_eq!(metric(&t, "bee_lands_total{result=\"landed\"}").await, 1.0);
    // The retry of a landed stack: skipped.
    let r = t.land(&a).await.expect("retry");
    assert_eq!(r["landed"][0]["skipped"], true, "{r}");
    assert_eq!(metric(&t, "bee_lands_total{result=\"skipped\"}").await, 1.0);
    // A conflicting stack: conflict.
    let (tip, _) = t.tip("main").await;
    let x = t.commit(&tip, &[("c.txt", b"one\n")], 2, "x").await;
    let y = t.commit(&tip, &[("c.txt", b"two\n")], 3, "y").await;
    t.land(&id_text(&x["id"])).await.expect("x");
    let e = t.land(&id_text(&y["id"])).await.expect_err("conflict");
    assert_eq!(e.reason, "MERGE_CONFLICT");
    assert_eq!(
        metric(&t, "bee_lands_total{result=\"conflict\"}").await,
        1.0
    );
    // A denied stack: denied (a required check that never ran).
    t.land_policy(
        "[[land]]\nref = \"main\"\nrequired_checks = [\"test\"]\n",
        5,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let z = t.commit(&tip, &[("z.txt", b"z\n")], 6, "z").await;
    let e = t.land(&id_text(&z["id"])).await.expect_err("denied");
    assert_eq!(e.reason, "POLICY_DENIED", "{e:?}");
    assert_eq!(metric(&t, "bee_lands_total{result=\"denied\"}").await, 1.0);
    let _ = (root, Value::Null);
}

/// F410-002: derived types carry a content-bearing flag (values depend on blob contents).
/// Quarantine keeps derived records; purging an object deletes the records of every
/// content-bearing type in each repo that referenced it, while content-free types (graph,
/// path history) stay. A later read derives again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_deletes_content_bearing_derived_records() {
    let t = start_default().await;
    let types = t
        .call("QueryService", "ListDerivedTypes", json!({}))
        .await
        .expect("types");
    let flag = |name: &str| {
        types["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == name)
            .map(|x| x["contentBearing"] == true)
            .unwrap_or_else(|| panic!("{name} listed: {types}"))
    };
    for name in ["blame", "diff_class", "copy_detection", "language_stats"] {
        assert!(flag(name), "{name} is content-bearing");
    }
    for name in ["graph", "path_history"] {
        assert!(!flag(name), "{name} is not content-bearing");
    }
    let (cs, blob) = land_file(&t, "src/lib.rs", b"pub fn secret() -> u32 { 42 }\n", 1).await;
    let get = |ty: &'static str, derive: bool| {
        let t = &t;
        let cs = cs.clone();
        async move {
            t.call(
                "QueryService",
                "GetDerived",
                json!({ "repo": repo(), "type": ty, "key": id_json(&cs), "deriveIfMissing": derive }),
            )
            .await
        }
    };
    for ty in ["diff_class", "language_stats", "copy_detection"] {
        let v = get(ty, true).await.expect("derive");
        assert_eq!(v["contentBearing"], true, "{v}");
    }
    let g = get("graph", false).await.expect("eager graph row");
    assert_ne!(g["contentBearing"], true);
    // Quarantine: the records stay (an unredaction must find them).
    t.call(
        "AdminService",
        "Redact",
        json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "leak" }),
    )
    .await
    .expect("quarantine");
    for ty in ["diff_class", "language_stats", "copy_detection"] {
        get(ty, false).await.expect("kept while quarantined");
    }
    // Purge: every content-bearing record of the repo goes.
    t.call(
        "AdminService",
        "PurgeNow",
        json!({ "repo": repo(), "ids": [id_json(&blob)], "reason": "purge" }),
    )
    .await
    .expect("purge");
    for ty in ["diff_class", "language_stats", "copy_detection"] {
        let e = get(ty, false).await.expect_err("deleted by the purge");
        assert_eq!(e.reason, "NOT_FOUND", "{ty}: {e:?}");
    }
    get("graph", false).await.expect("graph rows stay");
    // Derived again on demand, from what is still readable.
    get("diff_class", true).await.expect("re-derived");
}

fn agent_token(t: &TestServer, name: &str, owner: &str) -> String {
    let state = &t.srv.state;
    let agent = bee_auth::Principal::agent(
        bee_server::util::new_ulid(state.now_ms())
            .unwrap()
            .to_text(),
        name,
        bee_auth::AgentInfo {
            owner: bee_policy::PrincipalId::new(owner),
            runtime: "claude-code".into(),
            purpose: "tests".into(),
        },
    );
    t.token_for(&agent, &[owner])
}

/// F422-015, F422-017, F422-022: rate limiting is a token bucket per principal and per
/// delegation root (rate r per second, burst 2r). A request costs one token; `GetObjects` and
/// `HasObjects` cost one more per full thousand ids. A refusal is `RESOURCE_EXHAUSTED` with
/// the quota that was hit and `retry_after_ms`, and spends nothing; agents under one human
/// share that human's root bucket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limits_are_token_buckets_with_bulk_costs_and_shared_roots() {
    // One token a second, burst two, for every principal and every root.
    let t = start_with(|c| {
        c.admission.per_principal_rps = 1;
        c.admission.per_root_rps = 1;
    })
    .await;
    t.call(
        "RepoService",
        "UpdateRepo",
        json!({ "repo": repo(), "visibility": "VISIBILITY_PUBLIC" }),
    )
    .await
    .expect("public");
    let ids: Vec<Value> = (0..1000u32)
        .map(|i| id_json(&bee_object::blob_id(&i.to_be_bytes()).to_text()))
        .collect();
    let has = |tok: String, ids: Vec<Value>| {
        let t = &t;
        async move {
            t.call_as(
                &tok,
                "ObjectService",
                "HasObjects",
                json!({ "repo": repo(), "ids": ids }),
            )
            .await
            .map(|_| ())
        }
    };
    let (_, single) = t.principal("human", "Sid Single", "").await;
    // A 1000-id read costs two tokens: the whole burst, so a second bulk read right after is
    // refused. At a cost of one it would pass: one token would be left, plus what refilled
    // (one a second). The refusal proves the cost while the first read takes under two
    // seconds; a slower run gets a fresh principal and another try.
    let mut refused = None;
    for attempt in 0..5 {
        let (_, bulk) = t
            .principal("human", &format!("Bea Bulk {attempt}"), "")
            .await;
        let started = std::time::Instant::now();
        has(bulk.clone(), ids.clone())
            .await
            .expect("first bulk read");
        if started.elapsed() > std::time::Duration::from_millis(1900) {
            continue;
        }
        refused = Some(
            has(bulk.clone(), ids.clone())
                .await
                .expect_err("the bucket is empty"),
        );
        break;
    }
    let e = refused.expect("a bulk read finished within two seconds");
    assert_eq!(e.reason, "RESOURCE_EXHAUSTED", "{e:?}");
    assert!(e.retryable);
    assert!(e.retry_after_ms >= 1, "{e:?}");
    assert!(format!("{e:?}").contains("per_principal_rps"), "{e:?}");
    // One-id reads cost one token: two fit in the same burst.
    has(single.clone(), ids[..1].to_vec()).await.expect("one");
    has(single.clone(), ids[..1].to_vec()).await.expect("two");
    let e = has(single.clone(), ids[..1].to_vec())
        .await
        .expect_err("third");
    assert_eq!(e.reason, "RESOURCE_EXHAUSTED");
    // After the advertised wait the bucket has refilled.
    tokio::time::sleep(std::time::Duration::from_millis(e.retry_after_ms + 50)).await;
    has(single.clone(), ids[..1].to_vec())
        .await
        .expect("after retry-after");

    // Two agents under one human share the human's root bucket.
    let (owner, _) = t.principal("human", "Olga Owner", "").await;
    let a = agent_token(&t, "agent-a", &owner);
    let b = agent_token(&t, "agent-b", &owner);
    let who = |tok: String| {
        let t = &t;
        async move {
            t.call_as(&tok, "AuthService", "WhoAmI", json!({}))
                .await
                .map(|_| ())
        }
    };
    who(a.clone()).await.expect("a one");
    who(a.clone()).await.expect("a two");
    // B has spent nothing of its own, but the root's burst is gone.
    let e = who(b.clone()).await.expect_err("root bucket empty");
    assert_eq!(e.reason, "RESOURCE_EXHAUSTED", "{e:?}");
    assert!(format!("{e:?}").contains("per_root_rps"), "{e:?}");
    // A is refused by its own bucket first.
    let e = who(a.clone()).await.expect_err("a's bucket empty");
    assert!(format!("{e:?}").contains("per_principal_rps"), "{e:?}");
    // The refusals spent nothing: after one token's refill, B gets exactly one call.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    who(b.clone()).await.expect("b after refill");
    let e = who(b.clone()).await.expect_err("one token only");
    assert!(format!("{e:?}").contains("per_root_rps"), "{e:?}");
}

fn find_file(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

/// F422-006: stored bytes that fail verification raise an alert: the read fails with
/// `INTEGRITY`, `bee_integrity_failures_total{source}` counts it, an `error` line from the
/// `integrity` component names the object (operators find it with QueryOpsLogs), and the
/// `integrity` health component reports it. fsck's digest violations alert the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupted_bytes_raise_integrity_alerts() {
    let t = start_default().await;
    // Big enough to live in the blob store, not inline.
    let content: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let (_, blob) = land_file(&t, "data.bin", &content, 1).await;
    let health = t
        .call(
            "OpsService",
            "GetOpsHealth",
            json!({ "includeComponents": true }),
        )
        .await
        .expect("health");
    let component = |h: &Value| {
        h["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "integrity")
            .cloned()
            .expect("integrity component")
    };
    assert_eq!(component(&health)["readiness"], "HEALTH_STATUS_OK");
    // Flip one byte of the stored file.
    let id = bee_core::ObjectId::parse(&blob).unwrap();
    let path = find_file(t.dir.path(), &bee_core::hex::encode(id.digest())).expect("blob file");
    let mut raw = std::fs::read(&path).unwrap();
    let mid = raw.len() / 2;
    raw[mid] ^= 0x40;
    std::fs::write(&path, raw).unwrap();
    let e = t
        .call(
            "ObjectService",
            "GetObjects",
            json!({ "repo": repo(), "ids": [id_json(&blob)] }),
        )
        .await
        .expect_err("corrupt");
    assert_eq!(e.reason, "INTEGRITY", "{e:?}");
    assert!(metric(&t, "bee_integrity_failures_total{source=\"blob_store\"}").await >= 1.0);
    let health = t
        .call(
            "OpsService",
            "GetOpsHealth",
            json!({ "includeComponents": true }),
        )
        .await
        .expect("health");
    let c = component(&health);
    assert_eq!(c["readiness"], "HEALTH_STATUS_DOWN", "{c}");
    assert_eq!(c["reason"], "integrity_failures");
    assert!(c["detail"].as_str().unwrap().contains(&blob), "{c}");
    assert_eq!(health["readiness"], "HEALTH_STATUS_DEGRADED");
    let logs = t
        .call(
            "OpsService",
            "QueryOpsLogs",
            json!({ "filter": { "components": ["integrity"] }, "newestFirst": true }),
        )
        .await
        .expect("logs");
    let entry = &logs["entries"][0];
    assert_eq!(entry["message"], "integrity check failed", "{logs}");
    assert_eq!(entry["level"], "OPS_LOG_LEVEL_ERROR");
    assert!(entry.to_string().contains(&blob), "{entry}");
    // fsck finds it too, and alerts with its own source.
    let fsck = t
        .call("AdminService", "RunFsck", json!({ "repo": repo() }))
        .await
        .expect("fsck");
    assert_ne!(fsck["clean"], true, "{fsck}");
    assert_eq!(fsck["violations"][0]["kind"], "digest_mismatch", "{fsck}");
    assert!(metric(&t, "bee_integrity_failures_total{source=\"fsck\"}").await >= 1.0);
}

/// F422-019: the server ignores fields it does not know (a newer client), answers a request
/// missing a required field with `INVALID_ARGUMENT` / `missing_field`, and routes by the full
/// package-qualified method, so a package it does not serve (a future `bee.v2` next to v1)
/// is `UNIMPLEMENTED` rather than a v1 call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_fields_are_ignored_and_missing_fields_are_named() {
    let t = start_default().await;
    let r = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": repo(), "name": "main", "fieldFromTheFuture": { "x": 1 } }),
        )
        .await
        .expect("unknown fields are ignored");
    assert_eq!(r["ref"]["name"], "main");
    let e = t
        .call("ObjectService", "ResolveRef", json!({ "name": "main" }))
        .await
        .expect_err("no repo");
    assert_eq!(e.reason, "INVALID_ARGUMENT", "{e:?}");
    assert_eq!(e.subcode, "missing_field", "{e:?}");
    let e = t
        .call("AdminService", "RunGc", json!({}))
        .await
        .expect_err("no org");
    assert_eq!(e.subcode, "missing_field", "{e:?}");
    // Package-qualified routing: v2 of a v1 service is not served (yet), v1 is.
    let auth = format!("Bearer {}", t.token);
    let (status, headers, body) = http(
        "POST",
        &format!("{}/bee.v2.ObjectService/GetServerInfo", t.url()),
        &[
            ("content-type", "application/json"),
            ("authorization", &auth),
        ],
        b"{}".to_vec(),
    )
    .await;
    let doc: Value = serde_json::from_slice(&body).expect("a Connect error");
    assert_eq!(status, 404, "{headers:?} {doc}");
    assert_eq!(doc["code"], "unimplemented", "{doc}");
    assert_eq!(
        doc["details"][0]["debug"]["reason"], "UNIMPLEMENTED",
        "{doc}"
    );
    let (status, _, _) = http(
        "POST",
        &format!("{}/bee.v1.ObjectService/GetServerInfo", t.url()),
        &[("content-type", "application/json")],
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(status, 200);
}

/// F422-007: repo ids are ULIDs (48-bit milliseconds from the server clock, then 80 random
/// bits), assigned once by the forge and shared with the VCS layer, so the id names the same
/// repo on every surface and never changes with its name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repo_ids_are_clock_ordered_ulids_shared_by_forge_and_vcs() {
    let t = start_default().await;
    let before = t.srv.state.now_ms();
    let a = t
        .call(
            "RepoService",
            "CreateRepo",
            json!({ "org": "dev", "name": "ids-a", "autoInit": true }),
        )
        .await
        .expect("create a");
    let b = t
        .call(
            "RepoService",
            "CreateRepo",
            json!({ "org": "dev", "name": "ids-b", "autoInit": true }),
        )
        .await
        .expect("create b");
    let id_of = |r: &Value| r["id"].as_str().expect("repo id").to_string();
    let (ida, idb) = (id_of(&a), id_of(&b));
    let ua = bee_core::Ulid::parse(&ida).expect("a ULID");
    let ub = bee_core::Ulid::parse(&idb).expect("a ULID");
    assert!(
        ua.timestamp_ms() >= before.saturating_sub(1000)
            && ua.timestamp_ms() <= t.srv.state.now_ms()
    );
    assert!(ua < ub, "ids sort by creation time");
    // The same id names the repo on the VCS surface.
    let by_id = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": { "id": ida }, "name": "main" }),
        )
        .await
        .expect("VCS by forge id");
    let by_name = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": { "fullName": "dev/ids-a" }, "name": "main" }),
        )
        .await
        .expect("VCS by name");
    assert_eq!(by_id["ref"]["target"], by_name["ref"]["target"]);
    // Renaming keeps the id.
    let renamed = t
        .call(
            "RepoService",
            "UpdateRepo",
            json!({ "repo": { "fullName": "dev/ids-a" }, "name": "ids-renamed" }),
        )
        .await
        .expect("rename");
    assert_eq!(id_of(&renamed), ida);
}

/// F422-018, failure class 4 (blob store unavailable): object reads fail with a retryable
/// `UNAVAILABLE`, uploads fail without leaving an object row (so nothing can reference bytes
/// that are not durable), metadata reads keep answering, and everything recovers when the
/// store does, with fsck clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_store_outage_fails_retryably_and_leaves_nothing_dangling() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(dir.path());
    let faults = FaultInjector::new();
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = true;
    opts.faults = Some(faults.clone());
    let srv = start(opts).await.expect("server");
    let token = srv.dev.as_ref().unwrap().token.clone();
    let t = TestServer {
        srv,
        token,
        dir,
        cfg,
    };
    let (tip, blob) = land_file(&t, "kept.txt", b"kept\n", 1).await;
    faults.add(FaultRule::always("blob.*", Fault::unavailable()));
    let e = t
        .read("kept.txt", "main")
        .await
        .expect_err("bytes unreachable");
    assert_eq!(e.reason, "UNAVAILABLE", "{e:?}");
    assert!(e.retryable);
    let fresh = b"written during the outage\n";
    let fresh_id = bee_object::blob_id(fresh).to_text();
    let e = t
        .call(
            "WriteService",
            "UploadObjects",
            json!({ "messages": [{ "repo": repo(), "id": id_json(&fresh_id), "offset": "0", "data": B64.encode(fresh), "last": true }] }),
        )
        .await
        .expect_err("upload fails");
    assert_eq!(e.reason, "UNAVAILABLE", "{e:?}");
    assert!(e.retryable);
    // No row was written: a changeset cannot reference the bytes.
    let e = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": repo(),
                "parents": [id_json(&tip)],
                "delta": [{ "path": B64.encode("new.txt"), "put": { "mode": "MODE_FILE", "blob": id_json(&fresh_id), "size": fresh.len().to_string() } }],
                "changeId": change_id(77),
                "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": "dangling",
            }),
        )
        .await
        .expect_err("no dangling reference");
    assert_eq!(e.reason, "MISSING_OBJECT", "{e:?}");
    // Metadata-only reads still answer.
    assert_eq!(t.tip("main").await.0, tip);
    t.call(
        "QueryService",
        "Log",
        json!({ "repo": repo(), "at": { "expr": "main" } }),
    )
    .await
    .expect("log needs no blob bytes");
    faults.clear_rules();
    assert_eq!(t.read("kept.txt", "main").await.unwrap(), b"kept\n");
    land_file(&t, "new.txt", fresh, 2).await;
    let fsck = t
        .call("AdminService", "RunFsck", json!({ "repo": repo() }))
        .await
        .expect("fsck");
    assert!(
        fsck["violations"].as_array().is_none_or(|v| v.is_empty()),
        "{fsck}"
    );
    let _ = blob;
}

/// A minimal OTLP/HTTP collector: records each request body, answers 200.
async fn collector() -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let got: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let sink = got.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                loop {
                    let mut chunk = [0u8; 8192];
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if buf.len() < end + 4 + len {
                            break;
                        }
                        let body = buf[end + 4..end + 4 + len].to_vec();
                        buf.drain(..end + 4 + len);
                        if head.starts_with("post /v1/traces")
                            && let Ok(v) = serde_json::from_slice(&body)
                        {
                            sink.lock().unwrap().push(v);
                        }
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                            .await;
                    }
                }
            });
        }
    });
    (url, got)
}

/// F422-013: OpenTelemetry tracing. A request with a W3C `traceparent` becomes a server span
/// in the caller's trace (its trace id, the caller's span as parent) and is exported as
/// OTLP/HTTP JSON to the configured collector with the service resource; the response's
/// `traceresponse` names the span; an unsampled caller's span is not exported; operators find
/// the span by trace id through GetOpsTrace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spans_join_the_callers_trace_and_export_over_otlp() {
    let (url, got) = collector().await;
    let t = start_with(|c| {
        c.tracing.otlp_endpoint = url.clone();
        c.tracing.export_interval_ms = 50;
        c.tracing.service_name = "bee-test".into();
    })
    .await;
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let parent = "00f067aa0ba902b7";
    let auth = format!("Bearer {}", t.token);
    let who = |traceparent: String| {
        let (url, auth) = (t.url().to_string(), auth.clone());
        async move {
            http(
                "POST",
                &format!("{url}/bee.v1.AuthService/WhoAmI"),
                &[
                    ("content-type", "application/json"),
                    ("authorization", &auth),
                    ("traceparent", &traceparent),
                ],
                b"{}".to_vec(),
            )
            .await
        }
    };
    let (status, headers, _) = who(format!("00-{trace}-{parent}-01")).await;
    assert_eq!(status, 200);
    let tr = headers
        .get("traceresponse")
        .expect("traceresponse")
        .to_str()
        .unwrap()
        .to_string();
    let parts: Vec<&str> = tr.split('-').collect();
    assert_eq!((parts[0], parts[1], parts[3]), ("00", trace, "01"), "{tr}");
    let span_id = parts[2].to_string();
    assert_eq!(span_id.len(), 16);
    // Not sampled by the caller: answered, traced locally, not exported.
    let quiet = "11111111111111111111111111111111";
    let (status, headers, _) = who(format!("00-{quiet}-{parent}-00")).await;
    assert_eq!(status, 200);
    assert!(headers["traceresponse"].to_str().unwrap().ends_with("-00"));
    let span = eventually(10, "the exported span", || {
        let got = got.clone();
        let span_id = span_id.clone();
        async move {
            let bodies = got.lock().unwrap().clone();
            bodies.iter().find_map(|b| {
                b["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()?
                    .iter()
                    .find(|s| s["spanId"] == span_id.as_str())
                    .map(|s| (b.clone(), s.clone()))
            })
        }
    })
    .await;
    let (body, span) = span;
    assert_eq!(span["traceId"], trace);
    assert_eq!(span["parentSpanId"], parent);
    assert_eq!(span["kind"], 2);
    assert_eq!(span["name"], "bee.v1.AuthService/WhoAmI");
    assert_eq!(span["status"]["code"], 1);
    let resource = body["resourceSpans"][0]["resource"]["attributes"].to_string();
    assert!(resource.contains("bee-test"), "{resource}");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let all = got
        .lock()
        .unwrap()
        .clone()
        .iter()
        .map(|b| b.to_string())
        .collect::<String>();
    assert!(!all.contains(quiet), "an unsampled span was exported");
    // Operators find the span by its trace id.
    let found = t
        .call("OpsService", "GetOpsTrace", json!({ "traceId": trace }))
        .await
        .expect("trace");
    assert_eq!(found["spans"][0]["spanId"], span_id.as_str());
    assert_eq!(found["spans"][0]["parentSpanId"], parent);
}

/// F422-008: operational validation before production through the API: ValidateOpsConfig with
/// `component = "production"` runs the checklist on a configuration and lists each failed item
/// as an error (blocking) or a warning.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn production_checklist_over_the_ops_api() {
    let t = start_default().await;
    let validate = |toml: &'static str| {
        let t = &t;
        async move {
            t.call(
                "OpsService",
                "ValidateOpsConfig",
                json!({ "toml": toml, "component": "production" }),
            )
            .await
            .expect("validate")
        }
    };
    let dev = validate("").await;
    assert_ne!(dev["valid"], true, "{dev}");
    let codes: Vec<String> = dev["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap().to_string())
        .collect();
    for code in [
        "production:metadata_postgres",
        "production:blobs_s3",
        "production:tls",
    ] {
        assert!(codes.contains(&code.to_string()), "{code}: {codes:?}");
    }
    let ready = validate(
        "[server]\npublic_url = \"https://bee.example\"\ntrust_forwarded_for = true\n\
         [metadata]\nbackend = \"postgres\"\npostgres_url = \"postgres://db/bee\"\n\
         postgres_replica_url = \"postgres://replica/bee\"\n\
         [blobs]\nbackend = \"s3\"\ns3_bucket = \"bee\"\nurl_key_hex = \"2222222222222222222222222222222222222222222222222222222222222222\"\n\
         [auth]\nsigning_key_hex = \"1111111111111111111111111111111111111111111111111111111111111111\"\n\
         [ops]\noperators = [\"01J0OPERATOR\"]\n[backup]\ntarget = \"s3\"\n",
    )
    .await;
    assert_eq!(ready["valid"], true, "{ready}");
    assert!(
        ready["errors"].as_array().is_none_or(|e| e.is_empty()),
        "{ready}"
    );
    // Plain validation errors still come first.
    let broken = validate("[metadata]\nbackend = 3\n").await;
    assert_ne!(broken["valid"], true);
}

/// Calls `service/method` on the first live server of `urls`, moving to the next on any
/// transport or retryable failure, until `deadline`.
async fn call_any(
    urls: &std::sync::RwLock<Vec<String>>,
    token: &str,
    service: &str,
    method: &str,
    req: &Value,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut i = 0usize;
    loop {
        let url = {
            let u = urls.read().unwrap();
            u[i % u.len()].clone()
        };
        let mut c = bee_mcp::ClientConfig::new(url);
        c.token = Some(token.to_string());
        c.request_timeout = std::time::Duration::from_secs(10);
        match bee_mcp::RpcClient::new(&c)
            .call_rpc_json(service, method, req)
            .await
        {
            Ok(v) => return v,
            Err(e) if e.retryable || e.reason == "UNAVAILABLE" || e.reason == "INTERNAL" => {}
            Err(e) => panic!("{service}/{method}: {e:?}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{service}/{method} never succeeded"
        );
        i += 1;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// F422-009: a rolling upgrade under load. Two server processes share one cell's stores while
/// writers commit and land through whichever answers; each process in turn is stopped and
/// replaced by one running a new configuration (a config rollout), so old and new run side by
/// side. Every land succeeds exactly once, the ref log stays continuous, fsck is clean, and the
/// rollout is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn rolling_upgrade_under_load_loses_and_duplicates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let old_cfg = test_config(dir.path());
    let mut new_cfg = old_cfg.clone();
    new_cfg.land.batch_limit = 64;
    let boot = |cfg: bee_server::config::Config| async move {
        let mut opts = ServerOptions::new(cfg);
        opts.dev = true;
        start(opts).await.expect("server starts")
    };
    let a = boot(old_cfg.clone()).await;
    let b = boot(old_cfg.clone()).await;
    let token = a.dev.as_ref().unwrap().token.clone();
    let urls = Arc::new(std::sync::RwLock::new(vec![a.url.clone(), b.url.clone()]));
    const WRITERS: u64 = 4;
    const EACH: u64 = 15;
    let mut tasks = Vec::new();
    for w in 0..WRITERS {
        let (urls, token) = (urls.clone(), token.clone());
        tasks.push(tokio::spawn(async move {
            for i in 0..EACH {
                let n = w * 100 + i;
                let content = format!("writer {w} change {i}\n");
                let blob = bee_object::blob_id(content.as_bytes()).to_text();
                call_any(
                    &urls,
                    &token,
                    "WriteService",
                    "UploadObjects",
                    &json!({ "messages": [{ "repo": repo(), "id": id_json(&blob), "offset": "0", "data": B64.encode(&content), "last": true }] }),
                )
                .await;
                let tip = call_any(
                    &urls,
                    &token,
                    "ObjectService",
                    "ResolveRef",
                    &json!({ "repo": repo(), "name": "main" }),
                )
                .await;
                let cs = call_any(
                    &urls,
                    &token,
                    "WriteService",
                    "CreateChangeset",
                    &json!({
                        "repo": repo(),
                        "parents": [tip["ref"]["target"]],
                        "delta": [{ "path": B64.encode(format!("w{w}/{i}.txt")), "put": { "mode": "MODE_FILE", "blob": id_json(&blob), "size": content.len().to_string() } }],
                        "changeId": change_id(1000 + n),
                        "author": "loader",
                        "authorTime": { "name": "loader", "time": "2026-10-18T00:00:00Z" },
                        "message": format!("w{w} c{i}"),
                    }),
                )
                .await;
                call_any(
                    &urls,
                    &token,
                    "WriteService",
                    "Land",
                    &json!({ "repo": repo(), "ref": "main", "head": cs["changeset"]["id"] }),
                )
                .await;
            }
        }));
    }
    // Roll: replace A, then B, while the writers run.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        tasks.iter().any(|t| !t.is_finished()),
        "the load runs during the roll"
    );
    a.shutdown().await;
    let a2 = boot(new_cfg.clone()).await;
    urls.write().unwrap()[0] = a2.url.clone();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        tasks.iter().any(|t| !t.is_finished()),
        "the load runs during the roll"
    );
    b.shutdown().await;
    let b2 = boot(new_cfg.clone()).await;
    urls.write().unwrap()[1] = b2.url.clone();
    for t in tasks {
        t.await.expect("writer");
    }
    // Every change landed exactly once and the ref log is continuous.
    let mut c = bee_mcp::ClientConfig::new(b2.url.clone());
    c.token = Some(token.clone());
    let client = bee_mcp::RpcClient::new(&c);
    let log = client
        .call_rpc_json(
            "RefService",
            "GetRefLog",
            &json!({ "repo": repo(), "name": "main", "page": { "pageSize": 1000 } }),
        )
        .await
        .expect("ref log");
    let entries = log["entries"].as_array().expect("entries");
    let mut landed = std::collections::BTreeMap::new();
    let mut prev_new: Option<Value> = None;
    for (k, e) in entries.iter().enumerate() {
        assert_eq!(
            e["version"],
            (k as u64 + 1).to_string(),
            "versions 1..n: {e}"
        );
        if let Some(p) = &prev_new {
            assert_eq!(&e["old"], p, "each entry starts where the last ended");
        }
        prev_new = Some(e["new"].clone());
        for l in e["landed"].as_array().cloned().unwrap_or_default() {
            *landed
                .entry(l["changeId"]["raw"].as_str().unwrap().to_string())
                .or_insert(0) += 1;
        }
    }
    for w in 0..WRITERS {
        for i in 0..EACH {
            let key = change_id(1000 + w * 100 + i)["raw"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(
                landed.get(&key),
                Some(&1),
                "writer {w} change {i} landed once"
            );
        }
    }
    let fsck = client
        .call_rpc_json("AdminService", "RunFsck", &json!({ "repo": repo() }))
        .await
        .expect("fsck");
    assert!(
        fsck["violations"].as_array().is_none_or(|v| v.is_empty()),
        "{fsck}"
    );
    // The new configuration was recorded as a rollout.
    let rollouts = client
        .call_rpc_json("OpsService", "ListOpsRollouts", &json!({}))
        .await
        .expect("rollouts");
    let config = rollouts["rollouts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == "config")
        .count();
    assert!(config >= 2, "initial config and the new one: {rollouts}");
    a2.shutdown().await;
    b2.shutdown().await;
}

/// F44-011: the generic derived-data RPC. `ListDerivedTypes` names every type with its mode,
/// key kind and content-bearing flag; `GetDerived` reads eager rows as stored, derives lazy
/// ones on request (`derive_if_missing`) and then serves the stored value, answers NOT_FOUND
/// for an absent value without `derive_if_missing` and for unknown types, and takes a changeset
/// plus a path for per-path types (blame), checking read access to that path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_derived_serves_every_type_through_one_rpc() {
    let t = start_default().await;
    let user = t.dev_user();
    t.land_policy(
        &format!("[[access]]\npath = \"/secret\"\nread = [\"{user}\"]\nwrite = [\"{user}\"]\n"),
        1,
    )
    .await;
    let (base, _) = land_file(&t, "src/a.rs", b"fn a() {}\n", 2).await;
    let c = t
        .commit(
            &base,
            &[
                ("src/a.rs", b"fn a() {}\nfn b() {}\n"),
                ("secret/k", b"k\n"),
            ],
            3,
            "b",
        )
        .await;
    let cs = id_text(&c["id"]);
    t.land(&cs).await.expect("land");
    let types = t
        .call("QueryService", "ListDerivedTypes", json!({}))
        .await
        .expect("types");
    let ty = |name: &str| {
        types["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name}: {types}"))
    };
    assert_eq!(ty("graph")["mode"], "eager");
    assert_eq!(ty("diff_class")["mode"], "lazy");
    assert_eq!(ty("git_mapping")["mode"], "importer");
    assert_eq!(ty("blame")["keyKind"], "history_node");
    let get = |req: Value| {
        let t = &t;
        async move { t.call("QueryService", "GetDerived", req).await }
    };
    // Eager: stored at commit time.
    let g = get(json!({ "repo": repo(), "type": "graph", "key": id_json(&cs) }))
        .await
        .expect("graph");
    assert_ne!(g["derivedNow"], true);
    assert!(!g["value"].as_str().unwrap_or_default().is_empty());
    // Lazy: absent until derived, then stored.
    let e = get(json!({ "repo": repo(), "type": "diff_class", "key": id_json(&cs) }))
        .await
        .expect_err("not derived yet");
    assert_eq!(e.reason, "NOT_FOUND");
    let d = get(json!({ "repo": repo(), "type": "diff_class", "key": id_json(&cs), "deriveIfMissing": true }))
        .await
        .expect("derive");
    assert_eq!(d["derivedNow"], true);
    assert!(d["valueJson"].as_str().unwrap().contains("highest"), "{d}");
    let again = get(json!({ "repo": repo(), "type": "diff_class", "key": id_json(&cs) }))
        .await
        .expect("stored");
    assert_eq!(again["value"], d["value"]);
    // Per-path: blame of a file at a changeset.
    let b = get(json!({ "repo": repo(), "type": "blame", "key": id_json(&cs), "path": B64.encode("src/a.rs"), "deriveIfMissing": true }))
        .await
        .expect("blame by path");
    assert_eq!(b["type"], "blame");
    assert!(!b["value"].as_str().unwrap_or_default().is_empty());
    let e = get(json!({ "repo": repo(), "type": "blame", "key": id_json(&cs), "path": B64.encode("nope.rs"), "deriveIfMissing": true }))
        .await
        .expect_err("no such path");
    assert_eq!(e.reason, "NOT_FOUND");
    // A reader who cannot read /secret cannot blame it.
    let (rid, reader) = t.principal("human", "Rita Reader", "").await;
    collaborate(&t, &rid, &reader, "REPO_ROLE_READ").await;
    let e = t
        .call_as(
            &reader,
            "QueryService",
            "GetDerived",
            json!({ "repo": repo(), "type": "blame", "key": id_json(&cs), "path": B64.encode("secret/k"), "deriveIfMissing": true }),
        )
        .await
        .expect_err("restricted path");
    assert_eq!(e.reason, "POLICY_DENIED", "{e:?}");
    let e = get(json!({ "repo": repo(), "type": "no_such_type", "key": id_json(&cs) }))
        .await
        .expect_err("unknown type");
    assert_eq!(e.reason, "NOT_FOUND");
}

/// F41-019: Land with `allow_empty`. A change that rebases to nothing (someone landed the same
/// content first) is refused with `EMPTY_CHANGE` by default; with `allow_empty` it lands
/// without a new changeset: the landed index records it with the current tip as its result, so
/// a retry is idempotent and the ref log names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn land_with_allow_empty_records_the_tip_as_the_result() {
    let t = start_default().await;
    let (base, _) = t.tip("main").await;
    let first = t.commit(&base, &[("x.txt", b"same\n")], 1, "first").await;
    let first = id_text(&first["id"]);
    t.land(&first).await.expect("first lands");
    let (tip, version) = t.tip("main").await;
    let dup = t
        .commit(&base, &[("x.txt", b"same\n")], 2, "duplicate")
        .await;
    let dup = id_text(&dup["id"]);
    let e = t.land(&dup).await.expect_err("empty");
    assert_eq!(e.reason, "EMPTY_CHANGE", "{e:?}");
    let land = |allow: bool| {
        let t = &t;
        let dup = dup.clone();
        async move {
            t.call(
                "WriteService",
                "Land",
                json!({ "repo": repo(), "ref": "main", "head": id_json(&dup), "allowEmpty": allow }),
            )
            .await
        }
    };
    let r = land(true).await.expect("lands with allow_empty");
    assert_eq!(id_text(&r["landed"][0]["source"]), dup);
    assert_eq!(
        id_text(&r["landed"][0]["result"]),
        tip,
        "the tip is the result"
    );
    assert_eq!(id_text(&r["ref"]["target"]), tip, "no new changeset");
    // Recorded in the landed index: a retry is skipped, and the ref log names it.
    let again = land(true).await.expect("retry");
    assert_eq!(again["landed"][0]["skipped"], true, "{again}");
    let log = t
        .call(
            "RefService",
            "GetRefLog",
            json!({ "repo": repo(), "name": "main", "fromVersion": (version + 1).to_string() }),
        )
        .await
        .expect("ref log");
    let entry = &log["entries"][0];
    assert_eq!(id_text(&entry["landed"][0]["source"]), dup, "{log}");
    assert_eq!(id_text(&entry["landed"][0]["result"]), tip);
}

/// F43-009: decoded-object caches keep reads serving during a metadata outage. History, trees
/// and file contents read before the primary went down are answered from the server's decoded
/// rows while it is down (refs are served stale, SPEC §15.1); reads of anything never read
/// fail with a retryable `UNAVAILABLE`, as do writes; past the staleness bound the rows that
/// can change are no longer served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decoded_rows_keep_reads_serving_while_metadata_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    cfg.metadata.stale_read_max_ms = 1_500;
    let faults = FaultInjector::new();
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = true;
    opts.faults = Some(faults.clone());
    let srv = start(opts).await.expect("server");
    let token = srv.dev.as_ref().unwrap().token.clone();
    let t = TestServer {
        srv,
        token,
        dir,
        cfg,
    };
    land_file(&t, "src/lib.rs", b"pub fn a() {}\n", 1).await;
    let (tip, _) = land_file(&t, "README.md", b"# hello\n", 2).await;
    let reads = || {
        let t = &t;
        async move {
            let log = t
                .call(
                    "QueryService",
                    "Log",
                    json!({ "repo": repo(), "from": { "expr": "main" } }),
                )
                .await?;
            let tree = t
                .call(
                    "QueryService",
                    "ListTree",
                    json!({ "repo": repo(), "at": { "expr": "main" }, "path": "" }),
                )
                .await?;
            let file = t.read("src/lib.rs", "main").await?;
            Ok::<_, bee_proto::v1::ErrorDetail>((log, tree, file))
        }
    };
    let (log, tree, file) = reads().await.expect("warm");
    // Uploaded, never referenced and never read: nothing about it is cached.
    let orphan = t.upload(&[b"never read anywhere\n"]).await.remove(0);
    assert_eq!(file, b"pub fn a() {}\n");
    // The primary goes away.
    faults.add(FaultRule::always("meta.*", Fault::unavailable()));
    let (log2, tree2, file2) = reads()
        .await
        .unwrap_or_else(|e| panic!("served from the decoded rows: {e:?}; {:?}", faults.fired()));
    assert_eq!(log2["changesets"], log["changesets"]);
    assert_eq!(tree2["entries"], tree["entries"]);
    assert_eq!(file2, file);
    assert_eq!(t.tip("main").await.0, tip, "the ref, stale");
    // Never read: unavailable, retryable. Writes fail the same way.
    let e = t
        .call(
            "ObjectService",
            "GetObjects",
            json!({ "repo": repo(), "ids": [id_json(&orphan)] }),
        )
        .await
        .expect_err("never read");
    assert_eq!(e.reason, "UNAVAILABLE", "{e:?}");
    assert!(e.retryable);
    let e = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": repo(),
                "parents": [id_json(&tip)],
                "delta": [{ "path": B64.encode("new.txt"), "put": { "mode": "MODE_FILE", "blob": id_json(&orphan), "size": "20" } }],
                "changeId": change_id(3),
                "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": "during the outage",
            }),
        )
        .await
        .expect_err("writes fail");
    assert_eq!(e.reason, "UNAVAILABLE", "{e:?}");
    // Past the bound, rows that can change (object states) are no longer trusted.
    tokio::time::sleep(std::time::Duration::from_millis(1_700)).await;
    let e = t
        .read("src/lib.rs", "main")
        .await
        .expect_err("past the bound");
    assert_eq!(e.reason, "UNAVAILABLE", "{e:?}");
    // The primary is back: everything answers again.
    faults.clear_rules();
    assert_eq!(t.read("README.md", "main").await.unwrap(), b"# hello\n");
}

/// A merge changeset of `parents` putting `file` (its delta from the first parent).
async fn merge_commit(t: &TestServer, parents: &[&str], file: (&str, &[u8]), cid: u64) -> String {
    let id = t.upload(&[file.1]).await.remove(0);
    let r = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": repo(),
                "parents": parents.iter().map(|p| id_json(p)).collect::<Vec<_>>(),
                "delta": [{ "path": B64.encode(file.0), "put": { "mode": "MODE_FILE", "blob": id_json(&id), "size": file.1.len().to_string() } }],
                "changeId": change_id(cid),
                "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": "merge the side branch",
            }),
        )
        .await
        .expect("merge changeset");
    id_text(&r["changeset"]["id"])
}

/// F41-027: Land takes stacks that contain merges. The stack is the first-parent chain; a
/// merge member is rebased by replaying its delta from its first parent and keeps its other
/// parents, so the side branch lands with it. A land rule with `require_linear` refuses such
/// a stack with the `linear` requirement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stacks_that_contain_merges_land() {
    let t = start_default().await;
    let (base, _) = t.tip("main").await;
    let side = id_text(&t.commit(&base, &[("side.txt", b"side\n")], 1, "side").await["id"]);
    let f1 = id_text(&t.commit(&base, &[("a.txt", b"a\n")], 2, "f1").await["id"]);
    let m = merge_commit(&t, &[&f1, &side], ("side.txt", b"side\n"), 3).await;
    let top = id_text(
        &t.commit(&m, &[("b.txt", b"b\n")], 4, "after the merge")
            .await["id"],
    );
    // main moves meanwhile.
    land_file(&t, "x.txt", b"x\n", 5).await;
    let r = t.land(&top).await.expect("a stack with a merge lands");
    let landed = r["landed"].as_array().unwrap();
    assert_eq!(landed.len(), 3, "{r}");
    let merged = id_text(&landed[1]["result"]);
    let cs = t
        .call(
            "QueryService",
            "GetChangesets",
            json!({ "repo": repo(), "ids": [id_json(&merged)] }),
        )
        .await
        .expect("changeset");
    let parents: Vec<String> = cs["changesets"][0]["parents"]
        .as_array()
        .unwrap()
        .iter()
        .map(id_text)
        .collect();
    assert_eq!(parents.len(), 2, "{cs}");
    assert_eq!(parents[0], id_text(&landed[0]["result"]));
    assert_eq!(parents[1], side, "the side branch is kept");
    for (p, want) in [
        ("a.txt", &b"a\n"[..]),
        ("side.txt", b"side\n"),
        ("b.txt", b"b\n"),
        ("x.txt", b"x\n"),
    ] {
        assert_eq!(t.read(p, "main").await.unwrap(), want, "{p}");
    }
    // require_linear refuses merges.
    t.land_policy("[[land]]\nref = \"main\"\nrequire_linear = true\n", 6)
        .await;
    let (tip, _) = t.tip("main").await;
    let side2 = id_text(&t.commit(&base, &[("s2.txt", b"s2\n")], 7, "side 2").await["id"]);
    let m2 = merge_commit(&t, &[&tip, &side2], ("s2.txt", b"s2\n"), 8).await;
    let e = t.land(&m2).await.expect_err("not linear");
    assert_eq!(e.reason, "POLICY_DENIED", "{e:?}");
    assert!(format!("{e:?}").contains("linear"), "{e:?}");
}
