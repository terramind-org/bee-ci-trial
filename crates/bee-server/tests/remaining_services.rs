//! End-to-end tests of the services that closed the server's UNIMPLEMENTED surface
//! (`docs/spec/operations.md`): search (with its failure domain), policy and rulesets, ops,
//! the git gateway RPCs, the cross-domain comment API, audit sinks and exports, second
//! factors, backups and restores, and repo exports that the importer reads back. Each runs
//! on a `--dev` server (SQLite, or Postgres with `BEE_TEST_PG`) over HTTP/JSON and gRPC.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use bee_proto::v1;
use common::*;
use serde_json::{Value, json};

fn b64(s: &str) -> String {
    B64.encode(s.as_bytes())
}

fn unb64(v: &Value) -> Vec<u8> {
    B64.decode(v.as_str().unwrap_or("")).unwrap_or_default()
}

fn u64_of(v: &Value) -> u64 {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_u64())
        .unwrap_or(0)
}

async fn org_member(t: &TestServer, principal: &str, role: &str) {
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": principal, "role": role }),
    )
    .await
    .expect("set member");
}

/// Commits `files` on main and lands them; returns the new tip.
async fn land_files(t: &TestServer, files: &[(&str, &[u8])], cid: u64, msg: &str) -> String {
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, files, cid, msg).await["id"]);
    t.land(&head).await.expect("land");
    t.tip("main").await.0
}

fn search_cfg(c: &mut bee_server::config::Config) {
    c.search.poll_ms = 50;
    c.audit.sink_poll_ms = 50;
    c.audit.sink_retry_base_ms = 50;
    c.ops.metrics_sample_ms = 100;
}

// ----- search ------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_search_indexes_lands_and_respects_restricted_paths() {
    let t = start_with(search_cfg).await;
    let user = t.dev_user();
    t.land_policy(
        &format!("[[access]]\npath = \"/secret\"\nread = [\"{user}\"]\nwrite = [\"{user}\"]\n"),
        1,
    )
    .await;
    land_files(
        &t,
        &[
            ("pub/a.rs", b"fn charge_needle() {}\n"),
            (
                "secret/key.rs",
                b"const NEEDLE_KEY: &str = \"charge_needle\";\n",
            ),
        ],
        2,
        "needles",
    )
    .await;
    // The indexer picks the land up from the event log.
    let hits = eventually(20, "indexed hits", || async {
        let r = t
            .call(
                "SearchService",
                "SearchCode",
                json!({ "query": "charge_needle" }),
            )
            .await
            .ok()?;
        let hits = r["hits"].as_array().cloned().unwrap_or_default();
        (hits.len() == 2).then_some(hits)
    })
    .await;
    let mut paths: Vec<String> = hits
        .iter()
        .map(|h| String::from_utf8(unb64(&h["path"])).unwrap())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["pub/a.rs", "secret/key.rs"]);

    // A reader without access to /secret never sees it, counted or not. (Bob reads the
    // private repo as an org member, through the org's base role.)
    let (bob_id, bob) = t.principal("human", "bob", "").await;
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": bob_id, "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("bob joins dev");
    let r = t
        .call_as(
            &bob,
            "SearchService",
            "SearchCode",
            json!({ "query": "charge_needle", "includeFacets": true }),
        )
        .await
        .expect("bob searches");
    let hits = r["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{r}");
    assert_eq!(unb64(&hits[0]["path"]), b"pub/a.rs");
    assert_eq!(u64_of(&r["totalCount"]), 1);

    // Symbols, outline, definitions; over gRPC too.
    let syms = t
        .call(
            "SearchService",
            "SearchSymbols",
            json!({ "query": "charge_needle" }),
        )
        .await
        .expect("symbols");
    assert!(!syms["symbols"].as_array().unwrap().is_empty(), "{syms}");
    let outline = t
        .call(
            "SearchService",
            "GetDocumentSymbols",
            json!({ "repo": repo(), "path": b64("pub/a.rs") }),
        )
        .await
        .expect("outline");
    assert_eq!(outline["symbols"][0]["symbol"]["name"], "charge_needle");
    let mut c = v1::search_service_client::SearchServiceClient::new(t.grpc_channel().await);
    let defs = c
        .get_symbol_definitions(t.bearer(
            &t.token,
            v1::GetSymbolDefinitionsRequest {
                repo: prepo(),
                path: b"secret/key.rs".to_vec(),
                line: 1,
                column: 31,
                ..Default::default()
            },
        ))
        .await
        .expect("definitions over gRPC")
        .into_inner();
    assert_eq!(defs.definitions.len(), 1);
    assert_eq!(defs.definitions[0].path, b"pub/a.rs");
    // Stream every match.
    let mut s = c
        .stream_search_code(t.bearer(
            &t.token,
            v1::StreamSearchCodeRequest {
                query: "needle".into(),
                ..Default::default()
            },
        ))
        .await
        .expect("stream")
        .into_inner();
    let mut done = None;
    while let Some(m) = s.message().await.expect("stream message") {
        if let Some(v1::code_search_stream_message::Payload::Done(d)) = m.payload {
            done = Some(d);
        }
    }
    assert_eq!(done.expect("done message").files_matched, 2);
    // A bad query explains itself.
    let e = t
        .call(
            "SearchService",
            "SearchCode",
            json!({ "query": "(unbalanced" }),
        )
        .await
        .expect_err("bad query");
    assert_eq!(reason(&e), "INVALID_ARGUMENT");
    let p = t
        .call(
            "SearchService",
            "ParseSearchQuery",
            json!({ "kind": "SEARCH_QUERY_KIND_ISSUES", "query": "is:nope" }),
        )
        .await
        .expect("parse");
    assert_eq!(p["valid"], Value::Null);
    assert_eq!(p["errors"][0]["subcode"], "bad_value");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_search_saved_searches_trending_and_topics() {
    // Trending looks back from "now"; the fixture's changesets are dated COMMIT_TIME_MS.
    let t = start_at_fixture_time(|c| {
        search_cfg(c);
        c.search.featured_topics = vec!["rust".into()];
        c.search.collections = vec![bee_server::config::CollectionEntry {
            slug: "starter".into(),
            display_name: "Starter".into(),
            items: vec!["dev/tools".into(), "https://example.com".into()],
            ..Default::default()
        }];
    })
    .await;
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": "tools", "description": "build tooling", "topics": ["rust", "cli"] }),
    )
    .await
    .expect("repo");
    let issue = t
        .call(
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": "Flaky billing test", "body": "retries" }),
        )
        .await
        .expect("issue");
    let (tip, _) = t.tip("main").await;
    let head = id_text(
        &t.commit(&tip, &[("b.txt", b"b\n")], 7, "Fix billing flake")
            .await["id"],
    );
    t.call(
        "ChangeService",
        "OpenChange",
        json!({ "repo": repo(), "head": id_json(&head), "title": "Fix billing flake" }),
    )
    .await
    .expect("cr");
    let r = t
        .call(
            "SearchService",
            "SearchIssues",
            json!({ "query": "billing repo:dev/playground", "includeTextMatches": true }),
        )
        .await
        .expect("issues");
    assert_eq!(u64_of(&r["totalCount"]), 2, "{r}");
    let r = t
        .call(
            "SearchService",
            "SearchIssues",
            json!({ "query": "billing is:issue" }),
        )
        .await
        .expect("issues only");
    assert_eq!(u64_of(&r["totalCount"]), 1, "{r}");
    assert_eq!(r["hits"][0]["number"], issue["number"]);
    let r = t
        .call(
            "SearchService",
            "SearchIssues",
            json!({ "query": "billing is:pr" }),
        )
        .await
        .expect("crs only");
    assert_eq!(
        r["hits"][0]["subject"]["kind"],
        "SUBJECT_KIND_CHANGE_REQUEST"
    );

    let r = t
        .call(
            "SearchService",
            "SearchRepos",
            json!({ "query": "topic:rust" }),
        )
        .await
        .expect("repos");
    assert_eq!(r["hits"][0]["fullName"], "dev/tools", "{r}");
    let r = t
        .call(
            "SearchService",
            "SearchChangesets",
            json!({ "query": "flake" }),
        )
        .await
        .expect("changesets");
    // The changeset is not on main yet; land it and search again.
    assert_eq!(u64_of(&r["totalCount"]), 0, "{r}");
    t.land(&head).await.expect("land");
    let r = t
        .call(
            "SearchService",
            "SearchChangesets",
            json!({ "query": "flake" }),
        )
        .await
        .expect("changesets");
    assert_eq!(u64_of(&r["totalCount"]), 1, "{r}");
    let r = t
        .call(
            "SearchService",
            "SearchUsers",
            json!({ "query": "type:org dev" }),
        )
        .await
        .expect("users");
    assert_eq!(r["hits"][0]["login"], "dev", "{r}");

    // Saved searches: per principal, unique names, normalized queries.
    let s = t
        .call(
            "SearchService",
            "CreateSavedSearch",
            json!({ "name": "Mine", "kind": "SEARCH_QUERY_KIND_ISSUES", "query": "IS:open author:@me" }),
        )
        .await
        .expect("save");
    assert_eq!(s["query"], "is:open author:@me");
    let e = t
        .call(
            "SearchService",
            "CreateSavedSearch",
            json!({ "name": "mine", "kind": "SEARCH_QUERY_KIND_ISSUES", "query": "is:open" }),
        )
        .await
        .expect_err("duplicate");
    assert_eq!(reason(&e), "ALREADY_EXISTS");
    let id = s["id"].as_str().unwrap();
    t.call(
        "SearchService",
        "UpdateSavedSearch",
        json!({ "savedSearchId": id, "pinned": true }),
    )
    .await
    .expect("update");
    let l = t
        .call("SearchService", "ListSavedSearches", json!({}))
        .await
        .expect("list");
    assert_eq!(l["savedSearches"][0]["pinned"], true);
    let (_, bob) = t.principal("human", "bob", "").await;
    let e = t
        .call_as(
            &bob,
            "SearchService",
            "GetSavedSearch",
            json!({ "savedSearchId": id }),
        )
        .await
        .expect_err("not bob's");
    assert_eq!(reason(&e), "NOT_FOUND");
    t.call(
        "SearchService",
        "DeleteSavedSearch",
        json!({ "savedSearchId": id }),
    )
    .await
    .expect("delete");

    let tr = t
        .call(
            "SearchService",
            "ListTrendingRepos",
            json!({ "window": "TRENDING_WINDOW_WEEKLY" }),
        )
        .await
        .expect("trending");
    assert_eq!(tr["repos"][0]["repo"]["fullName"], "dev/playground", "{tr}");
    let dv = t
        .call(
            "SearchService",
            "ListTrendingDevelopers",
            json!({ "window": "TRENDING_WINDOW_WEEKLY" }),
        )
        .await
        .expect("developers");
    assert!(!dv["developers"].as_array().unwrap().is_empty(), "{dv}");
    let topic = t
        .call("SearchService", "GetTopic", json!({ "name": "rust" }))
        .await
        .expect("topic");
    assert_eq!(u64_of(&topic["repoCount"]), 1);
    assert_eq!(topic["featured"], true);
    let f = t
        .call("SearchService", "ListFeaturedTopics", json!({}))
        .await
        .expect("featured");
    assert_eq!(f["topics"][0]["name"], "rust");
    let st = t
        .call("SearchService", "SearchTopics", json!({ "query": "cli" }))
        .await
        .expect("topics");
    assert_eq!(st["topics"][0]["name"], "cli");
    let col = t
        .call(
            "SearchService",
            "GetTopicCollection",
            json!({ "slug": "starter" }),
        )
        .await
        .expect("collection");
    assert_eq!(col["items"].as_array().unwrap().len(), 2, "{col}");
    let e = t
        .call(
            "SearchService",
            "GetTopicCollection",
            json!({ "slug": "nope" }),
        )
        .await
        .expect_err("absent");
    assert_eq!(reason(&e), "NOT_FOUND");

    // Topics and repo search follow repo visibility (SPEC §9.6): `dev/tools` is private, so
    // an org member whose base role is none and an unrelated user see no trace of it.
    t.call(
        "OrgService",
        "UpdateOrgSettings",
        json!({ "org": "dev", "baseRepoRole": "REPO_ROLE_NONE" }),
    )
    .await
    .expect("base role none");
    let (member_id, member) = t.principal("human", "member", "").await;
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": member_id, "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("member");
    let (_, stranger) = t.principal("human", "stranger", "").await;
    let sees = |tok: String| {
        let t = &t;
        async move {
            let r = t
                .call_as(
                    &tok,
                    "SearchService",
                    "SearchRepos",
                    json!({ "query": "topic:rust" }),
                )
                .await
                .expect("search");
            let listed = r["hits"]
                .as_array()
                .is_some_and(|h| h.iter().any(|h| h["fullName"] == "dev/tools"));
            let topic = t
                .call_as(&tok, "SearchService", "GetTopic", json!({ "name": "rust" }))
                .await
                .expect("topic");
            (listed, u64_of(&topic["repoCount"]))
        }
    };
    assert_eq!(sees(t.token.clone()).await, (true, 1), "the owner");
    assert_eq!(
        sees(member.clone()).await,
        (false, 0),
        "a member without base role"
    );
    assert_eq!(
        sees(stranger.clone()).await,
        (false, 0),
        "an unrelated user"
    );
    t.call(
        "RepoService",
        "UpdateRepo",
        json!({ "repo": { "fullName": "dev/tools" }, "visibility": "VISIBILITY_PUBLIC" }),
    )
    .await
    .expect("made public");
    assert_eq!(
        sees(stranger).await,
        (true, 1),
        "public repos are listed for everyone"
    );
}

/// The index is a separate failure domain: with it broken, code search answers UNAVAILABLE
/// while every list, and forge search, keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_outage_leaves_lists_working() {
    let blocker = tempfile::tempdir().unwrap();
    let file = blocker.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let t = start_with(|c| {
        search_cfg(c);
        c.search.index_dir = file.join("search").display().to_string();
    })
    .await;
    t.call(
        "IssueService",
        "CreateIssue",
        json!({ "repo": repo(), "title": "still listed" }),
    )
    .await
    .expect("issue");
    let e = t
        .call(
            "SearchService",
            "SearchCode",
            json!({ "query": "anything" }),
        )
        .await
        .expect_err("index down");
    assert_eq!(reason(&e), "UNAVAILABLE");
    assert!(e.retryable);
    let l = t
        .call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect("issues list");
    assert_eq!(l["issues"].as_array().unwrap().len(), 1);
    t.call("ChangeService", "ListChanges", json!({ "repo": repo() }))
        .await
        .expect("changes list");
    let s = t
        .call(
            "SearchService",
            "SearchIssues",
            json!({ "query": "listed" }),
        )
        .await
        .expect("issue search does not use the code index");
    assert_eq!(u64_of(&s["totalCount"]), 1);
    land_files(&t, &[("x.txt", b"x\n")], 3, "x").await;
    let h = t
        .call(
            "OpsService",
            "GetOpsHealth",
            json!({ "includeComponents": true }),
        )
        .await
        .expect("health");
    let search = h["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "search")
        .cloned()
        .unwrap();
    assert_eq!(search["readiness"], "HEALTH_STATUS_DOWN", "{h}");
    assert_eq!(search["criticalPath"], Value::Null);
    assert_eq!(h["readiness"], "HEALTH_STATUS_DEGRADED");
}

// ----- policy ------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rulesets_are_enforced_by_land_and_reported() {
    let t = start_default().await;
    let (bob, _) = t.principal("human", "bob", "").await;
    // An evaluate-mode ruleset needing one approval: lands proceed and are recorded.
    let rs = t
        .call(
            "PolicyService",
            "CreateRuleset",
            json!({
                "repo": repo(), "name": "one-approval",
                "enforcement": "RULESET_ENFORCEMENT_EVALUATE",
                "conditions": { "refNameIncludes": ["~DEFAULT_BRANCH"] },
                "rules": [{ "requiredApprovals": { "count": 1 } }],
            }),
        )
        .await
        .expect("create ruleset");
    let id = rs["id"].as_str().unwrap().to_string();
    assert_eq!(u64_of(&rs["version"]), 1);
    land_files(&t, &[("a.txt", b"a\n")], 1, "a").await;
    let evals = eventually(10, "recorded evaluation", || async {
        let r = t
            .call(
                "PolicyService",
                "ListRulesetEvaluations",
                json!({ "repo": repo(), "rulesetId": id }),
            )
            .await
            .ok()?;
        let e = r["evaluations"].as_array().cloned().unwrap_or_default();
        (!e.is_empty()).then_some(e)
    })
    .await;
    assert_eq!(evals[0]["wouldBlock"], true, "{evals:?}");
    assert_eq!(evals[0]["blocked"], Value::Null);
    let doc = t
        .call(
            "PolicyService",
            "GetEffectivePolicy",
            json!({ "repo": repo() }),
        )
        .await
        .expect("effective");
    assert!(
        doc["normalizedJson"]
            .as_str()
            .unwrap()
            .contains(&format!("api-{id}"))
    );
    assert!(
        doc["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["kind"] == "POLICY_SOURCE_KIND_RULESET")
    );

    // Active: the same land is refused with the missing approval.
    let e = t
        .call(
            "PolicyService",
            "UpdateRuleset",
            json!({ "rulesetId": id, "expectedVersion": "7", "enforcement": "RULESET_ENFORCEMENT_ACTIVE" }),
        )
        .await
        .expect_err("stale version");
    assert_eq!(reason(&e), "VERSION_MISMATCH");
    t.call(
        "PolicyService",
        "UpdateRuleset",
        json!({ "rulesetId": id, "expectedVersion": "1", "enforcement": "RULESET_ENFORCEMENT_ACTIVE" }),
    )
    .await
    .expect("activate");
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("b.txt", b"b\n")], 2, "b").await["id"]);
    let e = t.land(&head).await.expect_err("blocked by the ruleset");
    assert_eq!(reason(&e), "POLICY_DENIED");
    let ev = t
        .call(
            "PolicyService",
            "EvaluatePolicyAction",
            json!({ "repo": repo(), "action": "land", "ref": "main", "paths": [b64("src/x.rs")], "diffClass": "logic" }),
        )
        .await
        .expect("evaluate");
    assert_eq!(ev["allowed"], Value::Null);
    assert!(
        ev["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m.as_str().unwrap().starts_with("approval")),
        "{ev}"
    );
    let ev = t
        .call(
            "PolicyService",
            "EvaluatePolicyAction",
            json!({ "repo": repo(), "action": "land", "ref": "main", "paths": [b64("src/x.rs")], "approverIds": [bob] }),
        )
        .await
        .expect("evaluate with approval");
    assert_eq!(ev["allowed"], true, "{ev}");
    let rules = t
        .call(
            "PolicyService",
            "ListRulesetRulesForRef",
            json!({ "repo": repo(), "ref": "main" }),
        )
        .await
        .expect("rules for ref");
    assert_eq!(rules["rules"][0]["rulesetId"], id.as_str());
    // A draft needing a check would have blocked the past land.
    let sim = t
        .call(
            "PolicyService",
            "SimulateRuleset",
            json!({ "draft": { "repo": repo(), "name": "ci", "rules": [{ "requiredChecks": { "checks": [{ "name": "ci" }] } }] } }),
        )
        .await
        .expect("simulate");
    assert!(u64_of(&sim["wouldBlockCount"]) >= 1, "{sim}");
    // Unsupported rule types are refused, not stored unenforced.
    let e = t
        .call(
            "PolicyService",
            "CreateRuleset",
            json!({ "repo": repo(), "name": "q", "rules": [{ "requiredDeployments": {} }] }),
        )
        .await
        .expect_err("unsupported");
    assert_eq!(e.subcode, "unsupported_rule");
    t.call("PolicyService", "DeleteRuleset", json!({ "rulesetId": id }))
        .await
        .expect("delete");
    t.land(&head)
        .await
        .expect("lands again once the ruleset is gone");

    // Validation, epochs, roles and permissions; gRPC too.
    let v = t
        .call(
            "PolicyService",
            "ValidatePolicy",
            json!({ "repo": repo(), "toml": "version = 1\n[members]\nbogus = 3\n" }),
        )
        .await
        .expect("validate");
    assert_eq!(v["valid"], Value::Null);
    assert!(!v["issues"].as_array().unwrap().is_empty());
    let mut c = v1::policy_service_client::PolicyServiceClient::new(t.grpc_channel().await);
    let ep = c
        .get_policy_epoch(t.bearer(&t.token, v1::GetPolicyEpochRequest { repo: prepo() }))
        .await
        .expect("epoch over gRPC")
        .into_inner();
    assert!(!ep.epoch.is_empty());
    let roles = c
        .list_builtin_roles(t.bearer(&t.token, v1::ListBuiltinRolesRequest::default()))
        .await
        .expect("builtin roles")
        .into_inner();
    assert!(roles.roles.iter().any(|r| r.name == "admin"));
    let role = t
        .call(
            "PolicyService",
            "CreateCustomRole",
            json!({ "org": "dev", "kind": "POLICY_ROLE_KIND_REPO", "name": "releaser", "baseRole": "write", "permissions": [] }),
        )
        .await
        .expect("custom role");
    let roles = t
        .call("PolicyService", "ListCustomRoles", json!({ "org": "dev" }))
        .await
        .expect("custom roles");
    assert_eq!(roles["roles"][0]["id"], role["id"]);
    let co = t
        .call(
            "PolicyService",
            "ValidateCodeowners",
            json!({ "repo": repo(), "content": "/src/ @alice\n[bad\n" }),
        )
        .await
        .expect("codeowners");
    assert_eq!(co["valid"], Value::Null);
    let sig = t
        .call(
            "PolicyService",
            "GetRequiredSignaturesStatus",
            json!({ "repo": repo(), "ref": "main", "changesets": [id_json(&t.tip("main").await.0)] }),
        )
        .await
        .expect("signatures");
    assert_eq!(sig["checks"][0]["state"], "POLICY_SIGNATURE_STATE_UNSIGNED");
}

// ----- ops ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ops_reports_health_metrics_logs_traces_and_status() {
    let t = start_with(|c| {
        search_cfg(c);
        c.blobs.s3_secret_access_key = "very-secret".into();
    })
    .await;
    land_files(&t, &[("o.txt", b"o\n")], 1, "o").await;
    let mut c = v1::ops_service_client::OpsServiceClient::new(t.grpc_channel().await);
    let h = c
        .get_ops_health(t.bearer(
            &t.token,
            v1::GetOpsHealthRequest {
                include_components: true,
                ..Default::default()
            },
        ))
        .await
        .expect("health over gRPC")
        .into_inner();
    assert_eq!(h.readiness, v1::HealthStatus::Ok as i32, "{h:?}");
    assert!(
        h.components
            .iter()
            .any(|c| c.name == "metadata" && c.critical_path)
    );
    let m = t
        .call(
            "OpsService",
            "QueryMetrics",
            json!({ "name": "bee_rpc_requests_total", "aggregation": "METRIC_AGGREGATION_SUM" }),
        )
        .await
        .expect("metrics");
    assert!(
        m["series"][0]["samples"][0]["value"].as_f64().unwrap() > 0.0,
        "{m}"
    );
    let e = t
        .call("OpsService", "QueryMetrics", json!({ "name": "nope" }))
        .await
        .expect_err("unknown metric");
    assert_eq!(e.subcode, "unknown_metric");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = t
        .call(
            "OpsService",
            "QueryMetricsRange",
            json!({ "name": "bee_lands_total", "stepSeconds": "1",
                    "start": "2020-01-01T00:00:00Z" }),
        )
        .await
        .expect_err("too many points");
    assert_eq!(reason(&r), "TOO_LARGE");
    let d = t
        .call(
            "OpsService",
            "ListMetricDescriptors",
            json!({ "prefix": "bee_land" }),
        )
        .await
        .expect("descriptors");
    assert!(d["descriptors"].as_array().unwrap().len() >= 3);
    let logs = t
        .call(
            "OpsService",
            "QueryOpsLogs",
            json!({ "filter": { "components": ["server"] } }),
        )
        .await
        .expect("logs");
    assert!(
        logs["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"] == "listening"),
        "{logs}"
    );
    let traces = t
        .call(
            "OpsService",
            "SearchOpsTraces",
            json!({ "operation": "Land" }),
        )
        .await
        .expect("traces");
    let trace_id = traces["traces"][0]["traceId"].as_str().unwrap().to_string();
    let tr = t
        .call("OpsService", "GetOpsTrace", json!({ "traceId": trace_id }))
        .await
        .expect("trace");
    assert!(tr["rootOperation"].as_str().unwrap().ends_with("/Land"));
    let cfg = t
        .call("OpsService", "GetEffectiveOpsConfig", json!({}))
        .await
        .expect("config");
    let toml = cfg["toml"].as_str().unwrap();
    assert!(!toml.contains("very-secret") && toml.contains("<redacted>"));
    let v = t
        .call(
            "OpsService",
            "ValidateOpsConfig",
            json!({ "toml": "[server]\nbogus = 1\n" }),
        )
        .await
        .expect("validate");
    assert_eq!(v["errors"][0]["code"], "unknown_key", "{v}");
    let rl = t
        .call("OpsService", "GetRateLimitStatus", json!({}))
        .await
        .expect("rate limits");
    assert_eq!(rl["buckets"].as_array().unwrap().len(), 2);
    let ro = eventually(10, "start rollout", || async {
        let r = t
            .call("OpsService", "ListOpsRollouts", json!({}))
            .await
            .ok()?;
        let v = r["rollouts"].as_array().cloned().unwrap_or_default();
        v.iter().find(|x| x["kind"] == "binary").cloned()
    })
    .await;
    let e = t
        .call("OpsService", "PauseOpsRollout", json!({ "id": ro["id"] }))
        .await
        .expect_err("finished");
    assert_eq!(reason(&e), "INVALID_STATE");
    // Status page.
    let inc = t
        .call(
            "OpsService",
            "CreateOpsIncident",
            json!({ "title": "Webhooks delayed", "impact": "minor", "componentIds": ["webhooks"],
                    "componentStatus": "OPS_COMPONENT_STATUS_DEGRADED", "body": "looking" }),
        )
        .await
        .expect("incident");
    let comps = t
        .call("OpsService", "ListOpsStatusComponents", json!({}))
        .await
        .expect("components");
    let w = comps["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "webhooks")
        .cloned()
        .unwrap();
    assert_eq!(w["status"], "OPS_COMPONENT_STATUS_DEGRADED");
    let inc = t
        .call(
            "OpsService",
            "UpdateOpsIncident",
            json!({ "id": inc["id"], "state": "OPS_INCIDENT_STATE_RESOLVED", "body": "fixed",
                    "componentStatus": "OPS_COMPONENT_STATUS_OPERATIONAL" }),
        )
        .await
        .expect("resolve");
    assert_eq!(inc["updates"].as_array().unwrap().len(), 2);
    let l = t
        .call(
            "OpsService",
            "ListOpsIncidents",
            json!({ "state": "OPS_INCIDENT_STATE_RESOLVED" }),
        )
        .await
        .expect("incidents");
    assert_eq!(l["incidents"].as_array().unwrap().len(), 1);
    // Operator RPCs need an operator.
    let (_, bob) = t.principal("human", "bob", "").await;
    let e = t
        .call_as(&bob, "OpsService", "GetEffectiveOpsConfig", json!({}))
        .await
        .expect_err("not an operator");
    assert_eq!(reason(&e), "POLICY_DENIED");
    t.call_as(&bob, "OpsService", "ListOpsStatusComponents", json!({}))
        .await
        .expect("the status page is public to members");
}

// ----- git gateway RPCs --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_rpcs_resolve_serve_plan_and_ingest() {
    let t = start_default().await;
    let tip = land_files(&t, &[("g/readme.md", b"# hello\n")], 1, "Add readme").await;
    let refs = t
        .call(
            "GitService",
            "GitListRefs",
            json!({ "repo": repo(), "symrefs": true }),
        )
        .await
        .expect("ls-refs");
    let main = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .cloned()
        .unwrap();
    let oid = main["oid"].as_str().unwrap().to_string();
    let head = refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "HEAD")
        .cloned()
        .unwrap();
    assert_eq!(head["symrefTarget"], "refs/heads/main");
    // Ids map both ways.
    let r = t
        .call(
            "GitService",
            "GitResolve",
            json!({ "repo": repo(), "queries": [{ "beeId": id_json(&tip) }, { "gitOid": oid }] }),
        )
        .await
        .expect("resolve");
    assert_eq!(r["results"][0]["gitOid"], oid.as_str());
    assert_eq!(id_text(&r["results"][1]["beeId"]), tip);
    assert_eq!(r["results"][1]["gitType"], "GIT_OBJECT_TYPE_COMMIT");
    // Object bytes hash back to the oid (gRPC stream).
    let mut c = v1::git_service_client::GitServiceClient::new(t.grpc_channel().await);
    let mut s = c
        .get_git_objects(t.bearer(
            &t.token,
            v1::GetGitObjectsRequest {
                repo: prepo(),
                oids: vec![oid.clone()],
                ..Default::default()
            },
        ))
        .await
        .expect("objects")
        .into_inner();
    let mut data = Vec::new();
    while let Some(ch) = s.message().await.expect("chunk") {
        assert!(ch.error_reason.is_empty(), "{ch:?}");
        data.extend(ch.data);
    }
    let h = bee_git::ObjectFormat::Sha1
        .hash_object(bee_git::ObjectType::Commit, &data)
        .unwrap();
    assert_eq!(h.to_hex(), oid);
    // Plan a full fetch; its pack lands in another repo through GitIngestCommits.
    let plan = t
        .call(
            "GitService",
            "GitPlanFetch",
            json!({ "repo": repo(), "wantRefs": ["refs/heads/main"], "done": true, "ofsDelta": true }),
        )
        .await
        .expect("plan");
    assert_eq!(plan["acks"]["ready"], true);
    let (status, _, pack) = http("GET", plan["packUrl"].as_str().unwrap(), &[], Vec::new()).await;
    assert_eq!(status, 200);
    assert!(pack.starts_with(b"PACK"));
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": "mirror" }),
    )
    .await
    .expect("mirror repo");
    let mirror = json!({ "fullName": "dev/mirror" });
    let zero = "0".repeat(40);
    let mrefs = t
        .call("GitService", "GitListRefs", json!({ "repo": mirror }))
        .await
        .expect("mirror refs");
    let old = mrefs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "refs/heads/main")
        .map(|r| r["oid"].as_str().unwrap().to_string())
        .unwrap_or(zero);
    let ing = t
        .call(
            "GitService",
            "GitIngestCommits",
            json!({ "repo": mirror, "commands": [{ "refName": "refs/heads/main", "oldOid": old, "newOid": oid }],
                    "packBytes": B64.encode(&pack) }),
        )
        .await
        .expect("ingest");
    assert_eq!(ing["unpackOk"], true, "{ing}");
    assert_eq!(ing["results"][0]["ok"], true, "{ing}");
    assert_eq!(ing["results"][0]["applied"], "GIT_PUSH_APPLY_KIND_LAND");
    assert_eq!(ing["results"][0]["resultOid"], oid.as_str());
    // Settings: LFS off is enforced; preflight explains an old git.
    t.call(
        "GitService",
        "UpdateGitConfig",
        json!({ "repo": repo(), "lfsEnabled": false }),
    )
    .await
    .expect("config");
    let e = t
        .call(
            "GitService",
            "GitLfsBatch",
            json!({ "repo": repo(), "operation": "GIT_LFS_OPERATION_DOWNLOAD",
                    "objects": [{ "oid": "a".repeat(64), "size": "1" }] }),
        )
        .await
        .expect_err("lfs off");
    assert_eq!(e.subcode, "lfs_disabled");
    let cfg = t
        .call("GitService", "GetGitConfig", json!({ "repo": repo() }))
        .await
        .expect("get config");
    assert_eq!(cfg["objectFormat"], "GIT_OBJECT_FORMAT_SHA1");
    assert_eq!(cfg["gitEnabled"], true);
    let pf = t
        .call(
            "GitService",
            "GitPreflight",
            json!({ "repo": repo(), "clientGitVersion": "2.20.0", "transport": "https" }),
        )
        .await
        .expect("preflight");
    assert_eq!(pf["ok"], Value::Null);
    assert!(
        pf["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["code"] == "git_too_old")
    );
    // A git credential reads but cannot write when read-only.
    let cred = t
        .call(
            "GitService",
            "CreateGitCredential",
            json!({ "repos": [repo()], "ttlSeconds": 600 }),
        )
        .await
        .expect("credential");
    let pw = cred["password"].as_str().unwrap();
    t.call_as(pw, "RefService", "ListRefs", json!({ "repo": repo() }))
        .await
        .expect("read with the credential");
    let (tip2, _) = t.tip("main").await;
    let e = t
        .call_as(
            pw,
            "WriteService",
            "Land",
            json!({ "repo": repo(), "ref": "main", "head": id_json(&tip2) }),
        )
        .await
        .expect_err("read-only credential");
    assert!(
        matches!(reason(&e), "POLICY_DENIED" | "PERMISSION_DENIED"),
        "{e:?}"
    );
    // Deploy keys resolve for an SSH frontend.
    let blob = [7u8; 51];
    let key = format!("ssh-ed25519 {}", B64.encode(blob));
    t.call(
        "RepoService",
        "AddDeployKey",
        json!({ "repo": repo(), "title": "ci", "publicKey": key, "readOnly": true }),
    )
    .await
    .expect("deploy key");
    let res = t
        .call(
            "GitService",
            "ResolveGitSshKey",
            json!({ "keyType": "ssh-ed25519", "publicKey": B64.encode(blob), "repo": repo(), "command": "git-upload-pack" }),
        )
        .await
        .expect("resolve key");
    assert_eq!(res["deployKey"], true);
    assert_eq!(res["readOnly"], true);
    t.call_as(
        res["sessionToken"].as_str().unwrap(),
        "RefService",
        "ListRefs",
        json!({ "repo": repo() }),
    )
    .await
    .expect("session token reads");
    let e = t
        .call(
            "GitService",
            "ResolveGitSshKey",
            json!({ "publicKey": B64.encode([1u8; 51]), "repo": repo() }),
        )
        .await
        .expect_err("unknown key");
    assert_eq!(reason(&e), "UNAUTHENTICATED");
    // A repo with git disabled refuses the gateway.
    t.call(
        "GitService",
        "UpdateGitConfig",
        json!({ "repo": repo(), "gitEnabled": false }),
    )
    .await
    .expect("disable git");
    let e = t
        .call("GitService", "GitListRefs", json!({ "repo": repo() }))
        .await
        .expect_err("git disabled");
    assert_eq!(reason(&e), "GIT_DISABLED");
}

// ----- comments ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn comment_service_routes_issue_and_change_comments() {
    let t = start_default().await;
    let (bob, btok) = t.principal("human", "bob", "").await;
    org_member(&t, &bob, "ROLE_MEMBER").await;
    let issue = t
        .call(
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": "t" }),
        )
        .await
        .expect("issue");
    let number = issue["number"].clone();
    let c = t
        .call(
            "IssueService",
            "CreateIssueComment",
            json!({ "repo": repo(), "number": number, "body": "first" }),
        )
        .await
        .expect("comment");
    let cid = c["id"].as_str().unwrap().to_string();
    let mut cc = v1::comment_service_client::CommentServiceClient::new(t.grpc_channel().await);
    let got = cc
        .get_comment(t.bearer(
            &t.token,
            v1::GetCommentRequest {
                comment_id: cid.clone(),
                ..Default::default()
            },
        ))
        .await
        .expect("get over gRPC")
        .into_inner();
    assert_eq!(got.body, "first");
    let e = t
        .call_as(
            &btok,
            "CommentService",
            "UpdateComment",
            json!({ "commentId": cid, "body": "hijack" }),
        )
        .await
        .expect_err("not the author");
    assert_eq!(reason(&e), "PERMISSION_DENIED");
    let u = t
        .call(
            "CommentService",
            "UpdateComment",
            json!({ "commentId": cid, "body": "second" }),
        )
        .await
        .expect("edit");
    assert_eq!(u["edited"], true);
    let edits = t
        .call(
            "CommentService",
            "ListCommentEdits",
            json!({ "commentId": cid }),
        )
        .await
        .expect("edits");
    assert_eq!(edits["edits"][0]["previousBody"], "first");
    let subject = json!({ "kind": "SUBJECT_KIND_COMMENT", "repo": repo(), "id": cid });
    t.call_as(
        &btok,
        "CommentService",
        "AddReaction",
        json!({ "subject": subject, "content": "REACTION_CONTENT_HEART" }),
    )
    .await
    .expect("react");
    let rs = t
        .call(
            "CommentService",
            "ListReactions",
            json!({ "subject": subject }),
        )
        .await
        .expect("reactions");
    assert_eq!(rs["groups"][0]["count"], 1, "{rs}");
    t.call_as(
        &btok,
        "CommentService",
        "RemoveReaction",
        json!({ "subject": subject, "content": "REACTION_CONTENT_HEART" }),
    )
    .await
    .expect("unreact");
    let issue_subject = json!({ "kind": "SUBJECT_KIND_ISSUE", "repo": repo(), "id": number });
    t.call(
        "CommentService",
        "AddReaction",
        json!({ "subject": issue_subject, "content": "REACTION_CONTENT_ROCKET" }),
    )
    .await
    .expect("react to the issue by number");
    let read = t
        .call(
            "CommentService",
            "MarkSubjectRead",
            json!({ "subject": issue_subject }),
        )
        .await
        .expect("read marker");
    assert!(read["readAt"].is_string());
    t.call(
        "CommentService",
        "DeleteComment",
        json!({ "commentId": cid }),
    )
    .await
    .expect("delete");
    let e = t
        .call("CommentService", "GetComment", json!({ "commentId": cid }))
        .await
        .expect_err("gone");
    assert_eq!(reason(&e), "NOT_FOUND");

    // Change request conversation comments route to the changes store.
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("c.txt", b"c\n")], 5, "c").await["id"]);
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&head), "title": "c" }),
        )
        .await
        .expect("cr");
    let crc = t
        .call(
            "ChangeService",
            "CreateChangeComment",
            json!({ "repo": repo(), "changeRequestId": cr["id"], "body": "looks good" }),
        )
        .await
        .expect("cr comment");
    let crc_id = crc["id"].as_str().unwrap().to_string();
    let g = t
        .call(
            "CommentService",
            "GetComment",
            json!({ "commentId": crc_id }),
        )
        .await
        .expect("get cr comment");
    assert_eq!(g["body"], "looks good");
    t.call(
        "CommentService",
        "UpdateComment",
        json!({ "commentId": crc_id, "body": "LGTM" }),
    )
    .await
    .expect("edit cr comment");
    let edits = t
        .call(
            "CommentService",
            "ListCommentEdits",
            json!({ "commentId": crc_id }),
        )
        .await
        .expect("cr edits");
    assert_eq!(edits["edits"][0]["previousBody"], "looks good");
    let s = json!({ "kind": "SUBJECT_KIND_COMMENT", "repo": repo(), "id": crc_id });
    t.call(
        "CommentService",
        "AddReaction",
        json!({ "subject": s, "content": "REACTION_CONTENT_THUMBS_UP" }),
    )
    .await
    .expect("react to cr comment");
    let rs = t
        .call("CommentService", "ListReactions", json!({ "subject": s }))
        .await
        .expect("cr comment reactions");
    assert_eq!(rs["reactions"].as_array().unwrap().len(), 1);
    let crs = json!({ "kind": "SUBJECT_KIND_CHANGE_REQUEST", "repo": repo(), "id": cr["number"] });
    t.call(
        "CommentService",
        "AddReaction",
        json!({ "subject": crs, "content": "REACTION_CONTENT_HOORAY" }),
    )
    .await
    .expect("react to the cr");
    let rs = t
        .call("CommentService", "ListReactions", json!({ "subject": crs }))
        .await
        .expect("cr reactions");
    assert_eq!(rs["groups"][0]["count"], 1);
    t.call(
        "CommentService",
        "DeleteComment",
        json!({ "commentId": crc_id }),
    )
    .await
    .expect("delete cr comment");
}

// ----- audit sinks and exports -------------------------------------------------------------

/// A local HTTP receiver that records request bodies and authorization headers.
async fn receiver() -> (String, Arc<Mutex<Vec<(String, String)>>>, Arc<Mutex<u32>>) {
    let got: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let fail_first = Arc::new(Mutex::new(1u32));
    let g = got.clone();
    let f = fail_first.clone();
    let app = axum::Router::new().route(
        "/ingest",
        axum::routing::post(move |headers: http::HeaderMap, body: String| {
            let g = g.clone();
            let f = f.clone();
            async move {
                // The first delivery fails: the sink must retry.
                {
                    let mut n = f.lock().unwrap();
                    if *n > 0 {
                        *n -= 1;
                        return http::StatusCode::SERVICE_UNAVAILABLE;
                    }
                }
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                g.lock().unwrap().push((auth, body));
                http::StatusCode::OK
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    (format!("http://{addr}/ingest"), got, fail_first)
}

fn verify_chain(entries: &[v1::AuditEntry]) {
    let mut prev: Option<Vec<u8>> = None;
    for e in entries {
        assert_eq!(
            bee_forge::audit::entry_hash(e),
            e.hash,
            "entry {} hash",
            e.seq
        );
        if let Some(p) = &prev {
            assert_eq!(&e.prev_hash, p, "entry {} link", e.seq);
        }
        prev = Some(e.hash.clone());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_sinks_deliver_with_retries_and_exports_verify() {
    let files = tempfile::tempdir().unwrap();
    let root = files.path().to_path_buf();
    let t = start_with(|c| {
        search_cfg(c);
        c.audit.file_roots = vec![root.display().to_string()];
    })
    .await;
    // Audited org activity.
    for i in 0..3 {
        t.call(
            "IssueService",
            "CreateIssue",
            json!({ "repo": repo(), "title": format!("i{i}") }),
        )
        .await
        .expect("issue");
    }
    let w = t.srv.forge.as_ref().unwrap();
    let org_id = t.srv.dev.as_ref().unwrap().org.0.to_text();
    w.ci.put_secret(
        &bee_forge::Cx::system(),
        &bee_forge::ci::secrets::VaultScope::Org(org_id.clone()),
        "SIEM_TOKEN",
        b"tok123",
        v1::SecretVisibility::All,
        Vec::new(),
    )
    .await
    .expect("org secret");
    let (url, got, _) = receiver().await;
    let sink = t
        .call(
            "AuditService",
            "CreateAuditSink",
            json!({ "org": "dev", "name": "siem", "kind": "AUDIT_SINK_KIND_HTTPS", "endpoint": url,
                    "credentialSecret": "SIEM_TOKEN", "startSeq": "1" }),
        )
        .await
        .expect("https sink");
    let dir = root.join("audit");
    let fsink = t
        .call(
            "AuditService",
            "CreateAuditSink",
            json!({ "org": "dev", "name": "archive", "kind": "AUDIT_SINK_KIND_S3",
                    "endpoint": format!("file://{}", dir.display()), "startSeq": "1",
                    "actionPrefixes": ["issue."] }),
        )
        .await
        .expect("file sink");
    let e = t
        .call(
            "AuditService",
            "CreateAuditSink",
            json!({ "org": "dev", "name": "siem", "kind": "AUDIT_SINK_KIND_HTTPS", "endpoint": "http://x" }),
        )
        .await
        .expect_err("duplicate name");
    assert_eq!(reason(&e), "ALREADY_EXISTS");
    let e = t
        .call(
            "AuditService",
            "CreateAuditSink",
            json!({ "org": "dev", "name": "evil", "kind": "AUDIT_SINK_KIND_S3", "endpoint": "file:///etc" }),
        )
        .await
        .expect_err("outside file roots");
    assert_eq!(e.subcode, "bad_endpoint");
    // The HTTPS sink recovers from the receiver's first failure and delivers in order.
    let delivered = eventually(20, "https delivery", || async {
        let g = got.lock().unwrap().clone();
        let entries: Vec<v1::AuditEntry> = g
            .iter()
            .flat_map(|(_, b)| {
                b.lines()
                    .map(|l| serde_json::from_str::<v1::AuditEntry>(l).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect();
        (entries.len() >= 3).then_some((g, entries))
    })
    .await;
    assert!(delivered.0.iter().all(|(a, _)| a == "Bearer tok123"));
    verify_chain(&delivered.1);
    assert_eq!(delivered.1[0].seq, 1);
    let s = t
        .call("AuditService", "GetAuditSink", json!({ "id": sink["id"] }))
        .await
        .expect("sink");
    assert_eq!(s["health"], "AUDIT_SINK_HEALTH_HEALTHY", "{s}");
    // The file sink wrote JSON-lines objects with only the matching actions.
    let wrote = eventually(20, "file sink objects", || async {
        let mut lines = Vec::new();
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let text = std::fs::read_to_string(e.path()).ok()?;
            lines.extend(text.lines().map(str::to_string));
        }
        (!lines.is_empty()).then_some(lines)
    })
    .await;
    for l in &wrote {
        let e: v1::AuditEntry = serde_json::from_str(l).unwrap();
        assert!(e.action.starts_with("issue."), "{}", e.action);
    }
    let test = t
        .call("AuditService", "TestAuditSink", json!({ "id": sink["id"] }))
        .await
        .expect("test sink");
    assert_eq!(test["delivered"], true, "{test}");
    let list = t
        .call("AuditService", "ListAuditSinks", json!({ "org": "dev" }))
        .await
        .expect("list sinks");
    assert_eq!(list["sinks"].as_array().unwrap().len(), 2);
    t.call(
        "AuditService",
        "UpdateAuditSink",
        json!({ "id": fsink["id"], "enabled": false }),
    )
    .await
    .expect("pause");
    let p = t
        .call("AuditService", "GetAuditSink", json!({ "id": fsink["id"] }))
        .await
        .expect("paused sink");
    assert_eq!(p["health"], "AUDIT_SINK_HEALTH_PAUSED");
    t.call(
        "AuditService",
        "DeleteAuditSink",
        json!({ "id": fsink["id"] }),
    )
    .await
    .expect("delete sink");

    // Exports: JSON lines with a head hash that matches the chain verifier.
    let mut ac = v1::audit_service_client::AuditServiceClient::new(t.grpc_channel().await);
    let ex = ac
        .export_audit_log(t.bearer(
            &t.token,
            v1::ExportAuditLogRequest {
                org: "dev".into(),
                format: v1::AuditExportFormat::Json as i32,
                ..Default::default()
            },
        ))
        .await
        .expect("export over gRPC")
        .into_inner();
    let ready = eventually(20, "export ready", || async {
        let r = t
            .call("AuditService", "GetAuditExport", json!({ "id": ex.id }))
            .await
            .ok()?;
        (r["state"] == "AUDIT_EXPORT_STATE_READY").then_some(r)
    })
    .await;
    let (status, _, body) =
        http("GET", ready["signedUrl"].as_str().unwrap(), &[], Vec::new()).await;
    assert_eq!(status, 200);
    let entries: Vec<v1::AuditEntry> = String::from_utf8(body)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(entries.len() as u64, u64_of(&ready["entryCount"]));
    verify_chain(&entries);
    assert_eq!(
        B64.encode(&entries.last().unwrap().hash),
        ready["headHash"].as_str().unwrap()
    );
    let v = t
        .call(
            "AuditService",
            "VerifyAuditChain",
            json!({ "org": "dev", "toSeq": ready["lastSeq"], "expectedHeadHash": ready["headHash"] }),
        )
        .await
        .expect("verify");
    assert_eq!(v["ok"], true, "{v}");
    let csv = t
        .call(
            "AuditService",
            "ExportAuditLog",
            json!({ "org": "dev", "format": "AUDIT_EXPORT_FORMAT_CSV" }),
        )
        .await
        .expect("csv export");
    let ready = eventually(20, "csv ready", || async {
        let r = t
            .call("AuditService", "GetAuditExport", json!({ "id": csv["id"] }))
            .await
            .ok()?;
        (r["state"] == "AUDIT_EXPORT_STATE_READY").then_some(r)
    })
    .await;
    let (_, _, body) = http("GET", ready["signedUrl"].as_str().unwrap(), &[], Vec::new()).await;
    let text = String::from_utf8(body).unwrap();
    assert!(text.starts_with("seq,id,time,"));
    assert_eq!(
        text.lines().count() as u64,
        u64_of(&ready["entryCount"]) + 1
    );
}

// ----- second factors ----------------------------------------------------------------------

fn totp_now(secret: &[u8], offset_steps: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    bee_server::services::second_factor::totp(secret, (now + offset_steps * 30) as u64)
}

fn secret_of(uri: &str) -> Vec<u8> {
    let s = uri
        .split("secret=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    bee_server::services::second_factor::base32_decode(s).unwrap()
}

async fn enroll_totp(t: &TestServer, token: &str) -> (Vec<u8>, String) {
    let b = t
        .call_as(
            token,
            "AuthService",
            "BeginSecondFactorEnrollment",
            json!({ "kind": "SECOND_FACTOR_KIND_TOTP", "nickname": "phone" }),
        )
        .await
        .expect("begin");
    let secret = secret_of(b["totpUri"].as_str().unwrap());
    let f = t
        .call_as(
            token,
            "AuthService",
            "FinishSecondFactorEnrollment",
            json!({ "enrollmentId": b["enrollmentId"], "totpCode": totp_now(&secret, 0) }),
        )
        .await
        .expect("finish");
    (secret, f["id"].as_str().unwrap().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn totp_enrollment_login_challenge_recovery_and_enforcement() {
    let t = start_default().await;
    let e = t
        .call(
            "AuthService",
            "BeginSecondFactorEnrollment",
            json!({ "kind": "SECOND_FACTOR_KIND_TOTP" }),
        )
        .await
        .expect("begin");
    let e2 = t
        .call(
            "AuthService",
            "FinishSecondFactorEnrollment",
            json!({ "enrollmentId": e["enrollmentId"], "totpCode": "000000" }),
        )
        .await;
    // A wrong code (almost surely) fails; the right one enrolls.
    let secret = secret_of(e["totpUri"].as_str().unwrap());
    if e2.is_err() {
        assert_eq!(e2.unwrap_err().subcode, "bad_code");
        t.call(
            "AuthService",
            "FinishSecondFactorEnrollment",
            json!({ "enrollmentId": e["enrollmentId"], "totpCode": totp_now(&secret, 0) }),
        )
        .await
        .expect("finish");
    }
    let rc = t
        .call(
            "AuthService",
            "BeginSecondFactorEnrollment",
            json!({ "kind": "SECOND_FACTOR_KIND_RECOVERY_CODES" }),
        )
        .await
        .expect("recovery codes");
    let codes: Vec<String> = rc["recoveryCodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert_eq!(codes.len(), 10);
    t.call(
        "AuthService",
        "FinishSecondFactorEnrollment",
        json!({ "enrollmentId": rc["enrollmentId"] }),
    )
    .await
    .expect("activate codes");
    let mut ac = v1::auth_service_client::AuthServiceClient::new(t.grpc_channel().await);
    let list = ac
        .list_second_factors(t.bearer(&t.token, v1::ListSecondFactorsRequest {}))
        .await
        .expect("list over gRPC")
        .into_inner();
    assert_eq!(list.factors.len(), 2);
    // Login now asks for the second factor instead of returning the token.
    let l = t
        .call("AuthService", "Login", json!({ "oidcIdToken": t.token }))
        .await
        .expect("login");
    assert!(l["token"].is_null(), "{l}");
    let ch = l["secondFactorChallengeId"].as_str().unwrap().to_string();
    let e = t
        .call(
            "AuthService",
            "VerifySecondFactor",
            json!({ "challengeId": ch, "totpCode": "999999" }),
        )
        .await;
    if let Err(e) = e {
        assert_eq!(reason(&e), "UNAUTHENTICATED");
    }
    let v = t
        .call(
            "AuthService",
            "VerifySecondFactor",
            json!({ "challengeId": ch, "totpCode": totp_now(&secret, 1) }),
        )
        .await
        .expect("verify");
    let elevated = v["token"].as_str().unwrap().to_string();
    t.call_as(&elevated, "AuthService", "WhoAmI", json!({}))
        .await
        .expect("elevated token works");
    // A recovery code answers once.
    let l = t
        .call("AuthService", "Login", json!({ "oidcIdToken": t.token }))
        .await
        .unwrap();
    let ch = l["secondFactorChallengeId"].as_str().unwrap().to_string();
    t.call(
        "AuthService",
        "VerifySecondFactor",
        json!({ "challengeId": ch, "recoveryCode": codes[0] }),
    )
    .await
    .expect("recovery code");
    let l = t
        .call("AuthService", "Login", json!({ "oidcIdToken": t.token }))
        .await
        .unwrap();
    let ch = l["secondFactorChallengeId"].as_str().unwrap().to_string();
    let e = t
        .call(
            "AuthService",
            "VerifySecondFactor",
            json!({ "challengeId": ch, "recoveryCode": codes[0] }),
        )
        .await
        .expect_err("used code");
    assert_eq!(reason(&e), "UNAUTHENTICATED");

    // The org requires 2FA: a member without it is refused; enrolling admits them.
    let (bob, btok) = t.principal("human", "bob", "").await;
    org_member(&t, &bob, "ROLE_MEMBER").await;
    t.call(
        "EnterpriseService",
        "UpdateTwoFactorRequirement",
        json!({ "scope": { "org": "dev" }, "required": true }),
    )
    .await
    .expect("require 2FA");
    let e = eventually(10, "2FA enforced", || async {
        t.call_as(
            &btok,
            "IssueService",
            "ListIssues",
            json!({ "repo": repo() }),
        )
        .await
        .err()
        .filter(|e| reason(e) == "TWO_FACTOR_REQUIRED")
    })
    .await;
    assert_eq!(reason(&e), "TWO_FACTOR_REQUIRED");
    let (_, bob_factor) = enroll_totp(&t, &btok).await;
    eventually(10, "admitted after enrolling", || async {
        t.call_as(
            &btok,
            "IssueService",
            "ListIssues",
            json!({ "repo": repo() }),
        )
        .await
        .ok()
    })
    .await;
    let e = t
        .call_as(
            &btok,
            "AuthService",
            "DeleteSecondFactor",
            json!({ "id": bob_factor }),
        )
        .await
        .expect_err("last authenticator");
    assert_eq!(reason(&e), "TWO_FACTOR_REQUIRED");
}

// ----- backups and restores ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backup_wipe_restore_keeps_history_changes_and_issues() {
    let backups = tempfile::tempdir().unwrap();
    let restores = tempfile::tempdir().unwrap();
    let (bdir, rdir) = (backups.path().to_path_buf(), restores.path().to_path_buf());
    let t = start_with(|c| {
        c.backup.target = format!("dir:{}", bdir.display());
        c.backup.restore_root = rdir.display().to_string();
    })
    .await;
    land_files(&t, &[("src/a.rs", b"fn a() {}\n")], 1, "a").await;
    land_files(&t, &[("src/b.rs", b"fn b() {}\n")], 2, "b").await;
    t.call(
        "IssueService",
        "CreateIssue",
        json!({ "repo": repo(), "title": "keep me" }),
    )
    .await
    .expect("issue");
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("c.txt", b"c\n")], 3, "c").await["id"]);
    t.call(
        "ChangeService",
        "OpenChange",
        json!({ "repo": repo(), "head": id_json(&head), "title": "keep this CR" }),
    )
    .await
    .expect("cr");
    let log_before = t
        .call(
            "QueryService",
            "Log",
            json!({ "repo": repo(), "from": { "expr": "main" } }),
        )
        .await
        .expect("log");
    let crs_before = t
        .call("ChangeService", "ListChanges", json!({ "repo": repo() }))
        .await
        .expect("changes");
    let issues_before = t
        .call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect("issues");

    let b = t
        .call(
            "AdminService",
            "CreateBackup",
            json!({ "note": "before wipe" }),
        )
        .await
        .expect("backup");
    let id = b["id"].as_str().unwrap().to_string();
    eventually(60, "backup done", || async {
        let l = t
            .call("AdminService", "ListBackups", json!({}))
            .await
            .ok()?;
        let b = l["backups"]
            .as_array()?
            .iter()
            .find(|x| x["id"] == id.as_str())?
            .clone();
        assert_ne!(b["state"], "BACKUP_STATE_FAILED", "{b}");
        (b["state"] == "BACKUP_STATE_SUCCEEDED").then_some(b)
    })
    .await;
    let mut adm = v1::admin_service_client::AdminServiceClient::new(t.grpc_channel().await);
    let r = adm
        .start_restore(t.bearer(
            &t.token,
            v1::StartRestoreRequest {
                backup_id: id.clone(),
                target_cell: "r1".into(),
                drill: true,
                ..Default::default()
            },
        ))
        .await
        .expect("restore over gRPC")
        .into_inner();
    let done = eventually(120, "restore done", || async {
        let g = t
            .call("AdminService", "GetRestore", json!({ "id": r.id }))
            .await
            .ok()?;
        (g["state"] != "BACKUP_STATE_RUNNING").then_some(g)
    })
    .await;
    assert_eq!(done["state"], "BACKUP_STATE_SUCCEEDED", "{done}");
    assert_eq!(done["fsckClean"], true, "{done}");
    assert_eq!(done["conformancePassed"], true, "{done}");
    let drills = t
        .call("OpsService", "ListOpsRestoreDrills", json!({}))
        .await
        .expect("drills");
    assert_eq!(drills["drills"][0]["passed"], true);
    let e = t
        .call(
            "AdminService",
            "StartRestore",
            json!({ "backupId": id, "targetCell": "r1" }),
        )
        .await
        .expect_err("target not empty");
    assert_eq!(reason(&e), "ALREADY_EXISTS");

    // Wipe: stop the server and delete its data directory.
    let cfg = t.cfg.clone();
    let data = cfg.server.data_dir.clone();
    t.srv.shutdown().await;
    std::fs::remove_dir_all(&data).unwrap();

    let restored = rdir.join("r1");
    let mut rcfg = test_config(&restored);
    if std::env::var("BEE_TEST_PG").is_ok() {
        rcfg.metadata.postgres_schema = bee_server::backup::pg_target_schema("r1");
    }
    let dir = tempfile::tempdir().unwrap();
    let t2 = start_cfg(dir, rcfg).await;
    let log_after = t2
        .call(
            "QueryService",
            "Log",
            json!({ "repo": repo(), "from": { "expr": "main" } }),
        )
        .await
        .expect("log after restore");
    assert_eq!(log_after["entries"], log_before["entries"]);
    let crs_after = t2
        .call("ChangeService", "ListChanges", json!({ "repo": repo() }))
        .await
        .expect("changes after restore");
    assert_eq!(crs_after["changes"], crs_before["changes"]);
    let issues_after = t2
        .call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect("issues after restore");
    assert_eq!(issues_after["issues"], issues_before["issues"]);
    assert_eq!(t2.read("src/a.rs", "main").await.unwrap(), b"fn a() {}\n");
    let f = t2
        .call("AdminService", "RunFsck", json!({ "org": "dev" }))
        .await
        .expect("fsck");
    assert_eq!(f["clean"], true, "{f}");
}

/// Base tables of a Postgres schema, by name.
async fn pg_tables(client: &tokio_postgres::Client, schema: &str) -> Vec<String> {
    client
        .query(
            "SELECT table_name FROM information_schema.tables
             WHERE table_schema = $1 AND table_type = 'BASE TABLE' ORDER BY table_name",
            &[&schema],
        )
        .await
        .expect("list tables")
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// Regression: restoring a Postgres-backed server failed with "table forge_activity of the
/// backup does not exist after migrations". The restore opened the target forge schema without
/// running the forge's migrations, so it had no tables to load the backup into. The restore must
/// recreate every forge table of the live schema and load the backup's rows into them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_restore_recreates_every_forge_table() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("skipped: set BEE_TEST_PG to a Postgres URL");
        return;
    };
    let backups = tempfile::tempdir().unwrap();
    let restores = tempfile::tempdir().unwrap();
    let (bdir, rdir) = (backups.path().to_path_buf(), restores.path().to_path_buf());
    let t = start_with(|c| {
        c.backup.target = format!("dir:{}", bdir.display());
        c.backup.restore_root = rdir.display().to_string();
    })
    .await;
    land_files(&t, &[("a.txt", b"a\n")], 1, "a").await;
    t.call(
        "IssueService",
        "CreateIssue",
        json!({ "repo": repo(), "title": "in the forge schema" }),
    )
    .await
    .expect("issue");
    let id = t
        .call("AdminService", "CreateBackup", json!({}))
        .await
        .expect("backup")["id"]
        .as_str()
        .unwrap()
        .to_string();
    eventually(60, "backup done", || async {
        let l = t
            .call("AdminService", "ListBackups", json!({}))
            .await
            .ok()?;
        let b = l["backups"]
            .as_array()?
            .iter()
            .find(|x| x["id"] == id.as_str())?
            .clone();
        assert_ne!(b["state"], "BACKUP_STATE_FAILED", "{b}");
        (b["state"] == "BACKUP_STATE_SUCCEEDED").then_some(b)
    })
    .await;
    // A cell of its own, so a rerun on the same database starts from empty target schemas.
    let cell = format!("pgforge{}", bee_server::util::random_hex(4));
    let r = t
        .call(
            "AdminService",
            "StartRestore",
            json!({ "backupId": id, "targetCell": cell, "drill": true }),
        )
        .await
        .expect("restore");
    let done = eventually(120, "restore done", || async {
        let g = t
            .call("AdminService", "GetRestore", json!({ "id": r["id"] }))
            .await
            .ok()?;
        (g["state"] != "BACKUP_STATE_RUNNING").then_some(g)
    })
    .await;
    assert_eq!(done["state"], "BACKUP_STATE_SUCCEEDED", "{done}");
    assert_eq!(done["fsckClean"], true, "{done}");
    // The detail counts what was loaded: "<n> metadata rows; <n> forge rows; ...".
    let detail = done["detail"].as_str().unwrap_or_default().to_string();
    let forge_rows: u64 = detail
        .split("; ")
        .find_map(|n| n.strip_suffix(" forge rows"))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no forge row count in {detail:?}"));
    assert!(forge_rows > 0, "{detail}");

    let client = bee_server::backup::sql::pg_connect(&url)
        .await
        .expect("connect");
    let live = pg_tables(
        &client,
        &format!("{}_forge", t.cfg.metadata.postgres_schema),
    )
    .await;
    let restored = pg_tables(
        &client,
        &format!("{}_forge", bee_server::backup::pg_target_schema(&cell)),
    )
    .await;
    assert!(live.iter().any(|t| t == "forge_activity"), "{live:?}");
    assert_eq!(
        restored, live,
        "the restored forge schema has the live schema's tables"
    );
}

// ----- exports -----------------------------------------------------------------------------

async fn main_oid(t: &TestServer, r: Value) -> String {
    let refs = t
        .call("GitService", "GitListRefs", json!({ "repo": r }))
        .await
        .expect("refs");
    refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "refs/heads/main")
        .map(|x| x["oid"].as_str().unwrap().to_string())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_archive_round_trips_through_the_importer() {
    let t = start_default().await;
    land_files(&t, &[("src/lib.rs", b"pub fn x() {}\n")], 1, "Add lib").await;
    land_files(&t, &[("README.md", b"# r\n")], 2, "Add readme").await;
    t.call(
        "IssueService",
        "CreateIssue",
        json!({ "repo": repo(), "title": "Exported issue", "body": "body" }),
    )
    .await
    .expect("issue");
    t.call(
        "IssueService",
        "CreateIssueComment",
        json!({ "repo": repo(), "number": "1", "body": "a comment" }),
    )
    .await
    .expect("comment");
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("cr.txt", b"cr\n")], 3, "cr").await["id"]);
    t.call(
        "ChangeService",
        "OpenChange",
        json!({ "repo": repo(), "head": id_json(&head), "title": "Exported CR" }),
    )
    .await
    .expect("cr");
    let ex = t
        .call(
            "ImportService",
            "StartExport",
            json!({ "org": "dev", "repos": [repo()], "format": "EXPORT_FORMAT_GITHUB_ARCHIVE", "includeMetadata": true }),
        )
        .await
        .expect("export");
    let done = eventually(60, "export done", || async {
        let g = t
            .call("ImportService", "GetExport", json!({ "id": ex["id"] }))
            .await
            .ok()?;
        let s = g["state"].as_str()?.to_string();
        (s == "EXPORT_STATE_SUCCEEDED" || s == "EXPORT_STATE_FAILED").then_some(g)
    })
    .await;
    assert_eq!(done["state"], "EXPORT_STATE_SUCCEEDED", "{done}");
    let (status, _, archive) = http(
        "GET",
        done["downloadUrl"].as_str().unwrap(),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(status, 200);
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("export.tar.gz");
    std::fs::write(&path, &archive).unwrap();
    // The importer's own archive reader understands it.
    let data = bee_github::archive::open(&path, "dev/playground", &work.path().join("x")).unwrap();
    assert!(data.skipped.is_empty(), "{:?}", data.skipped);
    assert_eq!(data.meta.issues.get(&1).unwrap().title, "Exported issue");
    assert_eq!(
        data.meta.pulls.values().next().unwrap().title,
        "Exported CR"
    );
    assert_eq!(
        data.meta.issue_comments.values().next().unwrap().body,
        "a comment"
    );
    let git_dir = data.git_dir.expect("bare repository in the archive");
    // Importing the bare repository reproduces every commit id.
    let imp = t
        .call(
            "ImportService",
            "StartImport",
            json!({ "sourceKind": "IMPORT_SOURCE_KIND_GIT", "source": "dev/roundtrip",
                    "sourceUrl": git_dir.display().to_string() }),
        )
        .await
        .expect("import");
    eventually(60, "import live", || async {
        let g = t
            .call("ImportService", "GetImport", json!({ "id": imp["id"] }))
            .await
            .ok()?;
        assert_ne!(g["state"], "IMPORT_STATE_FAILED", "{g}");
        (g["state"] == "IMPORT_STATE_LIVE").then_some(())
    })
    .await;
    let a = main_oid(&t, repo()).await;
    let b = main_oid(&t, json!({ "fullName": "dev/roundtrip" })).await;
    assert_eq!(a, b, "the exported history imports with identical git ids");
    let ls = t
        .call("ImportService", "ListExports", json!({ "org": "dev" }))
        .await
        .expect("list exports");
    assert_eq!(ls["exports"].as_array().unwrap().len(), 1);
    let e = t
        .call("ImportService", "CancelExport", json!({ "id": ex["id"] }))
        .await
        .expect_err("finished");
    assert_eq!(reason(&e), "INVALID_STATE");

    // A push mirror to a local bare repository carries the same ids.
    let remote = work.path().join("mirror.git");
    let ok = std::process::Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&remote)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        let ex = t
            .call(
                "ImportService",
                "StartExport",
                json!({ "org": "dev", "repos": [repo()], "format": "EXPORT_FORMAT_GIT_PUSH",
                        "remoteUrl": remote.display().to_string() }),
            )
            .await
            .expect("push export");
        let done = eventually(60, "push done", || async {
            let g = t
                .call("ImportService", "GetExport", json!({ "id": ex["id"] }))
                .await
                .ok()?;
            let s = g["state"].as_str()?.to_string();
            (s == "EXPORT_STATE_SUCCEEDED" || s == "EXPORT_STATE_FAILED").then_some(g)
        })
        .await;
        assert_eq!(done["state"], "EXPORT_STATE_SUCCEEDED", "{done}");
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&remote)
            .args(["rev-parse", "refs/heads/main"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), a);
    }
}

/// Regression (three-form conformance): `repo:`, `org:` and `user:` scope a discussion search
/// instead of filtering every discussion out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discussion_search_honours_scope_qualifiers() {
    let t = start_default().await;
    t.call(
        "RepoService",
        "UpdateRepoSettings",
        json!({"repo": repo(), "hasDiscussions": true}),
    )
    .await
    .expect("discussions on");
    let cat = t
        .call(
            "DiscussionService",
            "CreateDiscussionCategory",
            json!({"repo": repo(), "name": "general-q", "emoji": ":speech_balloon:",
                   "format": "DISCUSSION_CATEGORY_FORMAT_OPEN_ENDED"}),
        )
        .await
        .expect("category");
    t.call(
        "DiscussionService",
        "CreateDiscussion",
        json!({"repo": repo(), "categoryId": cat["id"], "title": "Scoped needle", "body": "b"}),
    )
    .await
    .expect("discussion");
    for query in [
        "needle",
        "needle repo:dev/playground",
        "needle org:dev",
        "needle user:dev",
    ] {
        let r = t
            .call(
                "SearchService",
                "SearchDiscussions",
                json!({ "query": query }),
            )
            .await
            .expect("search");
        let hits = r["hits"].as_array().map_or(0, Vec::len);
        assert_eq!(hits, 1, "{query}: {r}");
    }
}

/// Regression (three-form conformance): an export cancelled while it is still queued stays
/// cancelled; the job used to overwrite the cancellation with RUNNING and then SUCCEEDED.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_export_cancelled_while_queued_stays_cancelled() {
    let t = start_default().await;
    for _ in 0..5 {
        let ex = t
            .call(
                "ImportService",
                "StartExport",
                json!({ "org": "dev", "repos": [repo()], "format": "EXPORT_FORMAT_GITHUB_ARCHIVE" }),
            )
            .await
            .expect("export");
        let Ok(c) = t
            .call("ImportService", "CancelExport", json!({ "id": ex["id"] }))
            .await
        else {
            continue; // already finished: nothing to check this round
        };
        assert_eq!(c["state"], "EXPORT_STATE_CANCELLED", "{c}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let g = t
            .call("ImportService", "GetExport", json!({ "id": ex["id"] }))
            .await
            .expect("get export");
        assert_eq!(g["state"], "EXPORT_STATE_CANCELLED", "{g}");
    }
}

/// Regression (three-form conformance): the sink delivery worker records its progress with a
/// compare-and-set, so it never writes back a sink row read before an UpdateAuditSink.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audit_sink_updates_survive_the_delivery_worker() {
    let t = start_with(|c| c.audit.sink_poll_ms = 5).await;
    let sink = t
        .call(
            "AuditService",
            "CreateAuditSink",
            json!({ "org": "dev", "name": "racy", "kind": "AUDIT_SINK_KIND_HTTPS",
                    "endpoint": "http://127.0.0.1:9/ingest", "startSeq": "1" }),
        )
        .await
        .expect("sink");
    for i in 0..300 {
        let prefix = format!("race-{i}.");
        t.call(
            "AuditService",
            "UpdateAuditSink",
            json!({ "id": sink["id"], "actionPrefixes": [prefix.clone()] }),
        )
        .await
        .expect("update");
        tokio::time::sleep(Duration::from_millis(2)).await;
        let g = t
            .call("AuditService", "GetAuditSink", json!({ "id": sink["id"] }))
            .await
            .expect("get");
        assert_eq!(g["actionPrefixes"], json!([prefix]), "round {i}: {g}");
    }
}
