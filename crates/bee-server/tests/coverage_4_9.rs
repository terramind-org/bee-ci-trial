//! Coverage for PRODUCT §4.9 rows that had no test, through an in-process dev server:
//! `CheckObjectAccess` for caches (by object id, for a subject token), the error reasons the
//! redaction RPCs choose, how fast a revocation reaches a second server sharing the store
//! (the propagation bound), and tag rules applied to every way a tag is made or removed.

mod common;

use std::time::{Duration, Instant};

use bee_server::server::{ServerOptions, start};
use common::*;
use serde_json::json;

/// Lands a policy restricting `/secret` to the dev user, then a changeset with one open and
/// one secret file; returns their blob ids and the changeset id.
async fn seed(t: &TestServer) -> (String, String, String) {
    let user = t.dev_user();
    t.land_policy(
        &format!("[[access]]\npath = \"/secret\"\nread = [\"{user}\"]\nwrite = [\"{user}\"]\n"),
        1,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let c = t
        .commit(
            &tip,
            &[("open.txt", b"hello\n"), ("secret/key", b"hunter2\n")],
            2,
            "files",
        )
        .await;
    let cs = id_text(&c["id"]);
    t.land(&cs).await.expect("land");
    (
        bee_object::blob_id(b"hello\n").to_text(),
        bee_object::blob_id(b"hunter2\n").to_text(),
        cs,
    )
}

/// Makes `id` a member of `dev`, so it reads the private `dev/playground` through the org's
/// base role (SPEC §9.6).
async fn reader_of_dev(t: &TestServer, id: &str) {
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": id, "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("member");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn check_object_access_answers_per_object_for_the_subject_token() {
    let t = start_default().await;
    let (open, secret, _) = seed(&t).await;
    let unknown = bee_object::blob_id(b"never uploaded").to_text();
    let (other_id, other) = t.principal("human", "Rita Reader", "").await;
    reader_of_dev(&t, &other_id).await;
    // A cache asks on behalf of the reader: open content yes, restricted and unknown no, and
    // both of the latter look the same (no existence probe).
    let r = t
        .call(
            "AuthService",
            "CheckObjectAccess",
            json!({
                "repo": repo(),
                "subjectToken": other,
                "ids": [id_json(&open), id_json(&secret), id_json(&unknown)],
            }),
        )
        .await
        .expect("check");
    assert_eq!(r["allowed"], json!([true, false, false]), "{r}");
    assert_eq!(
        r["accessClasses"],
        json!([
            "ACCESS_CLASS_ROOT",
            "ACCESS_CLASS_RESTRICTED",
            "ACCESS_CLASS_RESTRICTED"
        ])
    );
    assert_eq!(r["principalId"], other_id);
    assert_eq!(r["cacheTtlSeconds"], 60);
    assert!(!r["policyEpoch"].as_str().unwrap_or("").is_empty());
    // The same question for the dev user, who may read `/secret`.
    let r = t
        .call(
            "AuthService",
            "CheckObjectAccess",
            json!({
                "repo": repo(),
                "subjectToken": t.token,
                "ids": [id_json(&open), id_json(&secret)],
            }),
        )
        .await
        .expect("check");
    assert_eq!(r["allowed"], json!([true, true]));
    // The secret blob is not open to everyone, so a shared cache may not serve it by class.
    assert_eq!(r["accessClasses"][1], "ACCESS_CLASS_RESTRICTED");
    // A bad subject token is refused.
    let e = t
        .call(
            "AuthService",
            "CheckObjectAccess",
            json!({ "repo": repo(), "subjectToken": "bee_garbage", "ids": [id_json(&open)] }),
        )
        .await
        .expect_err("bad subject");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redaction_errors_choose_specific_reasons() {
    let t = start_default().await;
    let (open, _, cs) = seed(&t).await;
    let call = |method: &'static str, ids: Vec<String>| {
        let t = &t;
        async move {
            t.call(
                "AdminService",
                method,
                json!({
                    "repo": repo(),
                    "ids": ids.iter().map(|i| id_json(i)).collect::<Vec<_>>(),
                    "reason": "test",
                }),
            )
            .await
        }
    };
    // Only blobs and metadata can be redacted.
    let e = call("Redact", vec![cs.clone()])
        .await
        .expect_err("changeset");
    assert_eq!(reason(&e), "NOT_REDACTABLE");
    // Unknown objects are not found.
    let unknown = bee_object::blob_id(b"never uploaded").to_text();
    let e = call("Redact", vec![unknown]).await.expect_err("unknown");
    assert_eq!(reason(&e), "NOT_FOUND");
    // Unredact and PurgeNow need a quarantined object.
    let e = call("Unredact", vec![open.clone()])
        .await
        .expect_err("live");
    assert_eq!(reason(&e), "NOT_QUARANTINED");
    let e = call("PurgeNow", vec![open.clone()])
        .await
        .expect_err("live");
    assert_eq!(reason(&e), "NOT_QUARANTINED");
    // Someone without the redact permission is denied by policy.
    let (other_id, other) = t.principal("human", "Rita Reader", "").await;
    reader_of_dev(&t, &other_id).await;
    let e = t
        .call_as(
            &other,
            "AdminService",
            "Redact",
            json!({ "repo": repo(), "ids": [id_json(&open)], "reason": "no" }),
        )
        .await
        .expect_err("not an admin");
    // Her repo role (read, the org's base role) does not grant `redact` (SPEC §9.6).
    assert_eq!(reason(&e), "PERMISSION_DENIED");
    // None of these is retryable; the happy path still works afterwards.
    assert!(!e.retryable);
    call("Redact", vec![open.clone()]).await.expect("redact");
    call("Unredact", vec![open]).await.expect("unredact");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_reaches_another_server_within_the_refresh_bound() {
    let t = start_default().await;
    let bound = Duration::from_millis(t.cfg.auth.revocation_refresh_ms);
    assert_eq!(bound, Duration::from_millis(1000), "the default bound");
    // A second server over the same data directory: another process in production.
    let mut opts = ServerOptions::new(t.cfg.clone());
    opts.dev = true;
    let second = start(opts).await.expect("second server");
    let mint = t
        .call(
            "AuthService",
            "MintToken",
            json!({ "scope": { "actions": ["read"] } }),
        )
        .await
        .expect("mint");
    let child = mint["token"].as_str().expect("token").to_string();
    let token_id = mint["tokenId"].as_str().unwrap_or_default().to_string();
    let who = |url: String, tok: String| async move {
        let mut c = bee_mcp::ClientConfig::new(url);
        c.token = Some(tok);
        bee_mcp::RpcClient::new(&c)
            .call_rpc_json("AuthService", "WhoAmI", &json!({}))
            .await
    };
    // A grandchild attenuated offline, never seen by any server.
    let grandchild = bee_auth::attenuate(
        &child,
        &bee_auth::Attenuation {
            scope: bee_auth::Scope::new().read_paths(["/docs"]).unwrap(),
            act_as: None,
        },
    )
    .unwrap()
    .text;
    for tok in [&child, &grandchild] {
        who(second.url.clone(), tok.clone())
            .await
            .expect("the second server accepts the derived tokens");
    }
    let revoked_at = Instant::now();
    t.call("AuthService", "Revoke", json!({ "tokenId": token_id }))
        .await
        .expect("revoke");
    // The revoking server refuses it at once ...
    let e = who(t.url().to_string(), child.clone())
        .await
        .expect_err("revoked here");
    assert_eq!(reason(&e), "TOKEN_REVOKED");
    // ... and the other one refuses the whole tree within the bound (the refresh interval,
    // plus scheduling slack).
    for tok in [&child, &grandchild] {
        loop {
            match who(second.url.clone(), tok.clone()).await {
                Err(e) => {
                    assert_eq!(reason(&e), "TOKEN_REVOKED");
                    break;
                }
                Ok(_) => {
                    assert!(
                        revoked_at.elapsed() < bound + Duration::from_millis(500),
                        "the second server still accepts a revoked token after the bound"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
    }
    // Tokens outside the revoked tree keep working on both.
    who(second.url.clone(), t.token.clone())
        .await
        .expect("root unaffected");
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tag_rules_govern_tags_from_refs_releases_and_the_tag_api() {
    let t = start_default().await;
    let user = t.dev_user();
    let (other_id, other) = t.principal("human", "Rel Ease", "").await;
    // A forge admin of the org, but only a writer under the repo policy.
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": other_id, "role": "ROLE_ADMIN" }),
    )
    .await
    .expect("member");
    t.land_policy(
        &format!(
            "[[refs]]\npattern = \"tags/*\"\ncreate = [\"{user}\"]\ndelete = [\"{user}\"]\n\
             [[refs]]\npattern = \"tags/nightly-*\"\ncreate = [\"@humans\"]\n"
        ),
        1,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let deny = |e: &bee_proto::v1::ErrorDetail| {
        assert_eq!(reason(e), "POLICY_DENIED", "{e:?}");
    };

    // RefService.
    let e = t
        .call_as(
            &other,
            "RefService",
            "CreateRef",
            json!({ "repo": repo(), "name": "tags/v0.1", "target": id_json(&tip) }),
        )
        .await
        .expect_err("not a releaser");
    deny(&e);
    t.call_as(
        &other,
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "tags/nightly-1", "target": id_json(&tip) }),
    )
    .await
    .expect("a more specific tag rule lets every human tag nightlies");
    t.call(
        "RefService",
        "CreateRef",
        json!({ "repo": repo(), "name": "tags/v0.1", "target": id_json(&tip) }),
    )
    .await
    .expect("the releaser tags");

    // The release tag API and releases go through the same rule.
    let e = t
        .call_as(
            &other,
            "ReleaseService",
            "CreateRepoTag",
            json!({ "repo": repo(), "name": "v0.2", "target": { "expr": "main" } }),
        )
        .await
        .expect_err("tag api");
    deny(&e);
    let e = t
        .call_as(
            &other,
            "ReleaseService",
            "CreateRelease",
            json!({ "repo": repo(), "tagName": "v0.3", "target": { "expr": "main" }, "name": "v0.3" }),
        )
        .await
        .expect_err("a published release would create the tag");
    deny(&e);
    // A release of an existing tag creates nothing, so it is allowed; deleting the tag with it
    // is not.
    let rel = t
        .call_as(
            &other,
            "ReleaseService",
            "CreateRelease",
            json!({ "repo": repo(), "tagName": "v0.1", "name": "first" }),
        )
        .await
        .expect("release of an existing tag");
    let e = t
        .call_as(
            &other,
            "ReleaseService",
            "DeleteRelease",
            json!({ "repo": repo(), "releaseId": rel["id"], "deleteTag": true }),
        )
        .await
        .expect_err("tag delete");
    deny(&e);
    let e = t
        .call_as(
            &other,
            "ReleaseService",
            "DeleteRepoTag",
            json!({ "repo": repo(), "name": "v0.1" }),
        )
        .await
        .expect_err("tag delete");
    deny(&e);
    // A draft creates no tag until it is published, and publishing is checked.
    let draft = t
        .call_as(
            &other,
            "ReleaseService",
            "CreateRelease",
            json!({ "repo": repo(), "tagName": "v0.4", "target": { "expr": "main" }, "draft": true }),
        )
        .await
        .expect("draft");
    let e = t
        .call_as(
            &other,
            "ReleaseService",
            "PublishRelease",
            json!({ "repo": repo(), "releaseId": draft["id"] }),
        )
        .await
        .expect_err("publish creates the tag");
    deny(&e);
    // The releaser may do all of it.
    t.call(
        "ReleaseService",
        "PublishRelease",
        json!({ "repo": repo(), "releaseId": draft["id"] }),
    )
    .await
    .expect("releaser publishes");
    t.call(
        "ReleaseService",
        "DeleteRelease",
        json!({ "repo": repo(), "releaseId": draft["id"], "deleteTag": true }),
    )
    .await
    .expect("releaser deletes the release and its tag");
    let tags = t
        .call("ReleaseService", "ListRepoTags", json!({ "repo": repo() }))
        .await
        .expect("tags");
    let names: Vec<&str> = tags["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["nightly-1", "v0.1"]);
}
