//! Identity and access end to end: service logins, token paste, default token lifetime, and
//! path restrictions across history, trees, diffs and object reads (SPEC §9).

mod common;

use base64::Engine as _;
use common::*;
use serde_json::json;

fn path_of(v: &serde_json::Value) -> String {
    String::from_utf8(B64.decode(v.as_str().unwrap()).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_login_token_paste_and_default_lifetime() {
    let hash = bee_auth::ServiceSecret::new("s3cret").hash().to_hex();
    let t = start_with(move |c| {
        c.auth.services.push(bee_server::config::ServiceEntry {
            name: "ci".into(),
            display: "CI".into(),
            org: String::new(),
            secret_sha256: hash,
        });
    })
    .await;
    let mut anon = bee_mcp::ClientConfig::new(t.url().to_string());
    anon.token = None;
    let anon = bee_mcp::RpcClient::new(&anon);
    let info = anon
        .call_rpc_json("ObjectService", "GetServerInfo", &json!({}))
        .await
        .expect("anonymous server info");
    assert_eq!(info["tier"], "server");
    assert!(!info["tokenPublicKeys"].as_array().unwrap().is_empty());
    let e = anon
        .call_rpc_json(
            "AuthService",
            "Login",
            &json!({ "service": { "name": "ci", "secret": "wrong" } }),
        )
        .await
        .expect_err("wrong secret");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
    let e = anon
        .call_rpc_json(
            "AuthService",
            "Login",
            &json!({ "service": { "name": "nobody", "secret": "s3cret" } }),
        )
        .await
        .expect_err("unknown service");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
    let login = anon
        .call_rpc_json(
            "AuthService",
            "Login",
            &json!({ "service": { "name": "ci", "secret": "s3cret" } }),
        )
        .await
        .unwrap();
    let token = login["token"].as_str().unwrap().to_string();
    let who = t
        .call_as(&token, "AuthService", "WhoAmI", json!({}))
        .await
        .unwrap();
    assert_eq!(who["principal"]["kind"], "PRINCIPAL_KIND_SERVICE");
    assert_eq!(who["principal"]["display"], "CI");
    // Default lifetime: 12 hours from minting.
    let ts: pbjson_types::Timestamp = serde_json::from_value(who["expiresAt"].clone()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        (ts.seconds - now - 12 * 3600).abs() < 120,
        "expires {:?}",
        who["expiresAt"]
    );
    // Token paste confirms a valid token without extending it.
    let pasted = anon
        .call_rpc_json("AuthService", "Login", &json!({ "oidcIdToken": t.token }))
        .await
        .unwrap();
    assert_eq!(pasted["token"], t.token);
    let e = anon
        .call_rpc_json("AuthService", "Login", &json!({ "githubStart": true }))
        .await
        .expect_err("not configured");
    assert_eq!(reason(&e), "UNIMPLEMENTED");
    let (status, _, body) = http("GET", &format!("{}/healthz", t.url()), &[], Vec::new()).await;
    assert_eq!((status, body.as_slice()), (200, b"ok\n".as_slice()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restricted_paths_are_filtered_from_history_trees_and_objects() {
    let t = start_default().await;
    let user = t.srv.dev.as_ref().unwrap().user.clone();
    let (root, _) = t.tip("main").await;
    let policy = format!(
        "version = 1\n[members]\nadmin = [\"{user}\"]\nwrite = [\"@humans\", \"@agents\"]\nread = [\"@everyone\"]\n\
         [[access]]\npath = \"/\"\nread = [\"@everyone\"]\nwrite = [\"@everyone\"]\n\
         [[access]]\npath = \"/secret\"\nread = [\"@humans\"]\nwrite = [\"@humans\"]\n\
         [[land]]\nref = \"main\"\nagents_may_land_without_human = [\"logic\", \"docs\"]\n"
    );
    let cs = t
        .commit(
            &root,
            &[
                (".beehive/policy.toml", policy.as_bytes()),
                ("secret/key.txt", b"hidden\n"),
                ("open.txt", b"open\n"),
            ],
            300,
            "mixed",
        )
        .await;
    t.land(&id_text(&cs["id"])).await.unwrap();
    let (tip, _) = t.tip("main").await;
    let only_secret = t
        .commit(
            &tip,
            &[("secret/key.txt", b"hidden 2\n")],
            301,
            "secret only",
        )
        .await;
    t.land(&id_text(&only_secret["id"])).await.unwrap();
    let (tip, _) = t.tip("main").await;
    let agent = t
        .call("AuthService", "CreateAgent", json!({ "display": "reader" }))
        .await
        .unwrap();
    let minted = t
        .call(
            "AuthService",
            "MintToken",
            json!({ "principalId": agent["id"] }),
        )
        .await
        .unwrap();
    let token = minted["token"].as_str().unwrap().to_string();
    let log = t
        .call_as(
            &token,
            "QueryService",
            "Log",
            json!({ "repo": repo(), "from": { "expr": "main" }, "includeMeta": true }),
        )
        .await
        .unwrap();
    let entries = log["changesets"].as_array().unwrap();
    // The secret-only changeset: filtered, no visible delta, meta hidden.
    assert_eq!(entries[0]["filtered"], true);
    assert!(entries[0]["delta"].as_array().is_none_or(|d| d.is_empty()));
    assert!(entries[0]["meta"].is_null());
    // The mixed changeset: filtered, readable entries kept, meta shown.
    assert_eq!(entries[1]["filtered"], true);
    assert_eq!(entries[1]["delta"].as_array().unwrap().len(), 2);
    assert_eq!(entries[1]["meta"]["message"], "mixed");
    // The human sees everything.
    let full = t
        .call(
            "QueryService",
            "Log",
            json!({ "repo": repo(), "from": { "expr": "main" } }),
        )
        .await
        .unwrap();
    assert!(full["changesets"][0]["filtered"].is_null());
    // Reads, listings, diffs and object fetches respect the restriction without leaking.
    let e = t
        .call_as(&token, "QueryService", "ReadPath", json!({ "repo": repo(), "at": { "expr": "main" }, "path": B64.encode("secret/key.txt") }))
        .await
        .expect_err("denied");
    assert_eq!(reason(&e), "POLICY_DENIED");
    let tree = t
        .call_as(
            &token,
            "QueryService",
            "ListTree",
            json!({ "repo": repo(), "at": { "expr": "main" }, "recursive": true }),
        )
        .await
        .unwrap();
    let listed: Vec<(String, bool)> = tree["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (path_of(&e["path"]), e["restricted"] == true))
        .collect();
    assert!(listed.contains(&("secret".to_string(), true)), "{listed:?}");
    assert!(
        !listed.iter().any(|(p, _)| p == "secret/key.txt"),
        "restricted subtrees are not descended: {listed:?}"
    );
    let diff = t
        .call_as(
            &token,
            "QueryService",
            "Diff",
            json!({ "repo": repo(), "from": { "expr": "main~2" }, "to": { "expr": "main" } }),
        )
        .await
        .unwrap();
    let diff_paths: Vec<String> = diff["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| path_of(&i["path"]))
        .collect();
    assert!(
        diff_paths.contains(&"open.txt".to_string()),
        "{diff_paths:?}"
    );
    assert!(
        !diff_paths.iter().any(|p| p.starts_with("secret")),
        "{diff_paths:?}"
    );
    let blob = bee_object::blob_id(b"hidden 2\n").to_text();
    let objs = t
        .call_as(
            &token,
            "ObjectService",
            "GetObjects",
            json!({ "repo": repo(), "ids": [id_json(&blob)] }),
        )
        .await
        .unwrap();
    assert_eq!(objs["items"][0]["status"], "ITEM_STATUS_NOT_FOUND");
    let has = t
        .call_as(
            &token,
            "ObjectService",
            "HasObjects",
            json!({ "repo": repo(), "ids": [id_json(&blob)] }),
        )
        .await
        .unwrap();
    assert_eq!(has["present"], json!([false]));
    // Writing there is refused for the agent too.
    let e = t
        .commit_as(&token, &tip, &[("secret/new.txt", b"x\n")], 302, "nope")
        .await
        .expect_err("write denied");
    assert_eq!(reason(&e), "POLICY_DENIED");
}

/// Scale-out (docs/scaling.md): the org admin list in the built-in org default policy is
/// changed through one server process and evaluated by another sharing the store. The other
/// process never sees the change's invalidation, so it may reuse what it read for at most
/// `forge.access_cache_ms`; before that bound it kept the old admin list until it restarted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_admin_changes_reach_other_processes_within_the_access_bound() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    cfg.forge.access_cache_ms = 300;
    let a = start_cfg(tempfile::tempdir().unwrap(), cfg.clone()).await;
    let b = start_cfg(tempfile::tempdir().unwrap(), cfg).await;
    let org_id = b.srv.dev.as_ref().unwrap().org;
    let org = b.srv.state.meta.get_org(org_id).await.unwrap().unwrap();
    let before = b.srv.state.org_text(&org).await.unwrap();
    let newcomer = "01JNEWADMIN0000000000000000";
    assert!(!before.contains(newcomer));
    // Process A makes the newcomer an org admin in the shared store.
    let key = format!("{}/{newcomer}", org_id.0.to_text()).into_bytes();
    assert!(
        a.srv
            .state
            .kv
            .apply(vec![bee_server::kv::KvOp::put(
                bee_server::auth::NS_ORG_ADMIN,
                key,
                b"1".to_vec()
            )])
            .await
            .unwrap()
    );
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let after = b.srv.state.org_text(&org).await.unwrap();
    assert!(
        after.contains(newcomer),
        "process B still serves the admin list it read before the change: {after}"
    );
}
