//! Repo access on every surface (SPEC §9.6, `docs/spec/server.md` §4.1): the forge's repo
//! record (visibility, archive, soft delete, transfer) and the principal's repo role decide
//! who reaches a repo through the VCS RPCs, git smart HTTP, LFS, the access-check RPCs,
//! search, forge event reads, MCP, the GitHub gateway, signed URLs and imports.

mod common;

use std::time::{Duration, Instant};

use base64::Engine as _;
use bee_proto::v1::ErrorDetail;
use common::*;
use serde_json::{Value, json};

/// Reasons the repo gate answers; any other failure means the call got past it.
const GATE: [&str; 5] = [
    "NOT_FOUND",
    "PERMISSION_DENIED",
    "POLICY_DENIED",
    "UNAUTHENTICATED",
    "ARCHIVED",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vis {
    Public,
    Internal,
    Private,
}

impl Vis {
    fn repo(self) -> &'static str {
        match self {
            Vis::Public => "pub",
            Vis::Internal => "int",
            Vis::Private => "priv",
        }
    }

    fn proto(self) -> &'static str {
        match self {
            Vis::Public => "VISIBILITY_PUBLIC",
            Vis::Internal => "VISIBILITY_INTERNAL",
            Vis::Private => "VISIBILITY_PRIVATE",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Who {
    Owner,
    Member,
    TeamMember,
    Outside,
    Stranger,
    ScopedAgent,
    Anonymous,
}

const WHO: [Who; 7] = [
    Who::Owner,
    Who::Member,
    Who::TeamMember,
    Who::Outside,
    Who::Stranger,
    Who::ScopedAgent,
    Who::Anonymous,
];

/// What a principal should get: `OK`, or the gate's reason.
fn expected(vis: Vis, who: Who, write: bool) -> &'static str {
    match who {
        Who::Anonymous => "UNAUTHENTICATED",
        Who::Owner | Who::TeamMember | Who::Outside => "OK",
        // The agent acts for the owner, but its token names only another repo.
        Who::ScopedAgent => "POLICY_DENIED",
        // Org member under base role "none": internal repos read, private ones invisible.
        Who::Member => match (vis, write) {
            (Vis::Private, _) => "NOT_FOUND",
            (_, true) => "PERMISSION_DENIED",
            (_, false) => "OK",
        },
        Who::Stranger => match (vis, write) {
            (Vis::Public, true) => "PERMISSION_DENIED",
            (Vis::Public, false) => "OK",
            _ => "NOT_FOUND",
        },
    }
}

/// What a caller without credentials gets on `surface` (`forge.allow_anonymous_public_read`,
/// on by default): the public read surfaces read a public repo and hide every other one; the
/// rest, and every write, still need a token. `None` = the same answer as [`expected`].
fn expected_anonymous(vis: Vis, surface: &str) -> Option<&'static str> {
    let public = vis == Vis::Public;
    match surface {
        // MCP tools call the same RPCs, so `query_log` reads exactly what `Log` does.
        "ObjectService.ResolveRef"
        | "QueryService.Log"
        | "RefService.ListRefs"
        | "MCP query_log" => Some(if public { "OK" } else { "NOT_FOUND" }),
        // git asks for credentials rather than being told a repo does not exist.
        "git upload-pack" | "LFS batch download" => Some(if public { "OK" } else { "401" }),
        "SearchService.SearchRepos" => Some(if public { "OK" } else { "HIDDEN" }),
        _ => None,
    }
}

/// The HTTP class of a reason (git, LFS, the GitHub gateway, MCP transport).
fn http_class(reason: &str) -> &'static str {
    match reason {
        "OK" => "OK",
        "NOT_FOUND" => "404",
        "UNAUTHENTICATED" => "401",
        "ARCHIVED" => "ARCHIVED",
        _ => "403",
    }
}

fn status_class(status: u16) -> String {
    match status {
        200..=299 => "OK".into(),
        s => s.to_string(),
    }
}

fn norm(r: Result<Value, ErrorDetail>) -> String {
    match r {
        Ok(_) => "OK".into(),
        Err(e) if GATE.contains(&e.reason.as_str()) => e.reason,
        Err(_) => "OK".into(),
    }
}

fn rref(name: &str) -> Value {
    json!({ "fullName": name })
}

fn b64(s: &str) -> String {
    B64.encode(s.as_bytes())
}

struct World {
    t: TestServer,
    tokens: Vec<(Who, Option<String>)>,
}

impl World {
    fn token(&self, who: Who) -> Option<&str> {
        self.tokens
            .iter()
            .find(|(w, _)| *w == who)
            .and_then(|(_, t)| t.as_deref())
    }
}

async fn create_repo(t: &TestServer, name: &str, vis: &str) -> Value {
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": name, "visibility": vis, "autoInit": true }),
    )
    .await
    .unwrap_or_else(|e| panic!("create {name}: {e:?}"))
}

async fn join_dev(t: &TestServer, id: &str) {
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": id, "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("org member");
}

/// Adds `id` as a collaborator with `role` on `repo` and accepts the invitation as them.
async fn collaborate(t: &TestServer, repo: &str, id: &str, token: &str, role: &str) {
    let r = t
        .call(
            "RepoService",
            "AddCollaborator",
            json!({ "repo": rref(repo), "principalId": id, "role": role }),
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

/// A token for an agent owned by the dev user, scoped to `repos`.
fn scoped_agent_token(t: &TestServer, repos: &[&str]) -> String {
    let state = &t.srv.state;
    let now = state.now_ms() / 1000;
    let agent = bee_auth::Principal::agent(
        bee_server::util::new_ulid(state.now_ms())
            .unwrap()
            .to_text(),
        "scoped",
        bee_auth::AgentInfo {
            owner: bee_policy::PrincipalId::new(t.dev_user()),
            runtime: "claude-code".into(),
            purpose: "tests".into(),
        },
    );
    state
        .ring
        .read()
        .unwrap()
        .mint(
            &bee_auth::MintRequest {
                principal: agent,
                on_behalf_of: vec![bee_auth::PrincipalRef::new(
                    t.dev_user(),
                    bee_policy::PrincipalKind::Human,
                )],
                scope: bee_auth::Scope::new()
                    .repos(repos.iter().copied())
                    .unwrap()
                    .expires_at(now + 1800),
            },
            now,
        )
        .unwrap()
        .text
}

/// `dev` with base role none; `dev/pub`, `dev/int`, `dev/priv` (and `dev/other`); an org
/// member, a team member with write on the three, an outside collaborator with write on the
/// three, an unrelated user and an agent of the owner whose token names `dev/other` only.
async fn world() -> World {
    let t = start_default().await;
    t.call(
        "OrgService",
        "UpdateOrgSettings",
        json!({ "org": "dev", "baseRepoRole": "REPO_ROLE_NONE" }),
    )
    .await
    .expect("base role none");
    for v in [Vis::Public, Vis::Internal, Vis::Private] {
        create_repo(&t, v.repo(), v.proto()).await;
    }
    create_repo(&t, "other", "VISIBILITY_PRIVATE").await;
    let (member_id, member) = t.principal("human", "member", "").await;
    join_dev(&t, &member_id).await;
    let (team_id, teamer) = t.principal("human", "teamer", "").await;
    join_dev(&t, &team_id).await;
    t.call(
        "TeamService",
        "CreateTeam",
        json!({ "org": "dev", "slug": "core", "name": "core" }),
    )
    .await
    .expect("team");
    t.call(
        "TeamService",
        "AddTeamMember",
        json!({ "org": "dev", "team": "core", "principalId": team_id }),
    )
    .await
    .expect("team member");
    let (outside_id, outside) = t.principal("human", "outside", "").await;
    let (_, stranger) = t.principal("human", "stranger", "").await;
    for v in [Vis::Public, Vis::Internal, Vis::Private] {
        let name = format!("dev/{}", v.repo());
        t.call(
            "TeamService",
            "GrantTeamRepoAccess",
            json!({ "org": "dev", "team": "core", "repo": rref(&name), "role": "write" }),
        )
        .await
        .expect("team grant");
        collaborate(&t, &name, &outside_id, &outside, "REPO_ROLE_WRITE").await;
    }
    let scoped = scoped_agent_token(&t, &["dev/other"]);
    let owner = t.token.clone();
    World {
        tokens: vec![
            (Who::Owner, Some(owner)),
            (Who::Member, Some(member)),
            (Who::TeamMember, Some(teamer)),
            (Who::Outside, Some(outside)),
            (Who::Stranger, Some(stranger)),
            (Who::ScopedAgent, Some(scoped)),
            (Who::Anonymous, None),
        ],
        t,
    }
}

async fn rpc(t: &TestServer, token: Option<&str>, svc: &str, m: &str, req: Value) -> String {
    norm(t.call_as(token.unwrap_or(""), svc, m, req).await)
}

fn basic(token: &str) -> String {
    format!("Basic {}", B64.encode(format!("git:{token}")))
}

async fn git_info_refs(t: &TestServer, token: Option<&str>, name: &str, write: bool) -> String {
    let service = if write {
        "git-receive-pack"
    } else {
        "git-upload-pack"
    };
    let url = format!("{}/{name}.git/info/refs?service={service}", t.url());
    let auth = token.map(basic);
    let mut h: Vec<(&str, &str)> = Vec::new();
    if let Some(a) = &auth {
        h.push(("authorization", a.as_str()));
    }
    let (status, _, body) = http("GET", &url, &h, Vec::new()).await;
    if status == 400 && String::from_utf8_lossy(&body).contains("ARCHIVED") {
        return "ARCHIVED".into();
    }
    status_class(status)
}

async fn lfs_batch(t: &TestServer, token: Option<&str>, name: &str, write: bool) -> String {
    let op = if write { "upload" } else { "download" };
    let url = format!("{}/{name}.git/info/lfs/objects/batch", t.url());
    let auth = token.map(basic);
    let mut h: Vec<(&str, &str)> = vec![
        ("accept", "application/vnd.git-lfs+json"),
        ("content-type", "application/vnd.git-lfs+json"),
    ];
    if let Some(a) = &auth {
        h.push(("authorization", a.as_str()));
    }
    let body = json!({ "operation": op, "transfers": ["basic"], "objects": [] });
    let (status, _, out) = http("POST", &url, &h, body.to_string().into_bytes()).await;
    if status == 400 && String::from_utf8_lossy(&out).contains("ARCHIVED") {
        return "ARCHIVED".into();
    }
    status_class(status)
}

async fn compat(
    t: &TestServer,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let auth = token.map(|t| format!("token {t}"));
    let mut h: Vec<(&str, &str)> = vec![
        ("accept", "application/vnd.github+json"),
        ("content-type", "application/json"),
    ];
    if let Some(a) = &auth {
        h.push(("authorization", a.as_str()));
    }
    let bytes = if body.is_null() {
        Vec::new()
    } else {
        body.to_string().into_bytes()
    };
    let (status, _, out) = http(method, &format!("{}{path}", t.url()), &h, bytes).await;
    (status, serde_json::from_slice(&out).unwrap_or(Value::Null))
}

/// One MCP `tools/call` (with its own session); `OK` or the gate reason it reports.
async fn mcp(t: &TestServer, token: Option<&str>, tool: &str, args: Value) -> String {
    let url = format!("{}/mcp", t.url());
    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers: Vec<(&str, &str)> = vec![
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
    ];
    if let Some(a) = &auth {
        headers.push(("authorization", a.as_str()));
    }
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } } });
    let (status, h, _) = http("POST", &url, &headers, init.to_string().into_bytes()).await;
    if status != 200 {
        return status_class(status);
    }
    let session = h
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    headers.push(("mcp-session-id", session.as_str()));
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    http("POST", &url, &headers, note.to_string().into_bytes()).await;
    let call = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": tool, "arguments": args } });
    let (status, _, body) = http("POST", &url, &headers, call.to_string().into_bytes()).await;
    if status != 200 {
        return status_class(status);
    }
    let text = String::from_utf8_lossy(&body).to_string();
    let doc = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .next_back()
        .map(str::trim)
        .unwrap_or(text.as_str())
        .to_string();
    let v: Value = serde_json::from_str(&doc).unwrap_or(Value::Null);
    let failed = v["result"]["isError"] == true || !v["error"].is_null();
    if !failed {
        return "OK".into();
    }
    let s = v.to_string();
    GATE.iter()
        .find(|r| s.contains(*r))
        .map(|r| r.to_string())
        .unwrap_or_else(|| "OK".into())
}

/// Uploads one blob into `name` (`commit`).
async fn upload(t: &TestServer, token: Option<&str>, name: &str, data: &[u8]) -> String {
    let id = bee_object::blob_id(data).to_text();
    let m = json!({ "repo": rref(name), "id": id_json(&id), "offset": "0", "data": B64.encode(data), "last": true });
    rpc(
        t,
        token,
        "WriteService",
        "UploadObjects",
        json!({ "messages": [m] }),
    )
    .await
}

async fn tip_of(t: &TestServer, name: &str) -> String {
    let r = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": rref(name), "name": "main" }),
        )
        .await
        .expect("resolve");
    id_text(&r["ref"]["target"])
}

/// Every probe for one (repo, principal) cell: (surface, got, want).
async fn probe_cell(w: &World, vis: Vis, who: Who) -> Vec<(String, String, String)> {
    let t = &w.t;
    let tok = w.token(who);
    let name = format!("dev/{}", vis.repo());
    let read = expected(vis, who, false);
    let write = expected(vis, who, true);
    let tip = tip_of(t, &name).await;
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut push = |surface: &str, got: String, want: &str| {
        let want = match who {
            Who::Anonymous => expected_anonymous(vis, surface).unwrap_or(want),
            _ => want,
        };
        out.push((surface.to_string(), got, want.to_string()));
    };
    let repo = rref(&name);

    push(
        "ObjectService.ResolveRef",
        rpc(
            t,
            tok,
            "ObjectService",
            "ResolveRef",
            json!({ "repo": repo, "name": "main" }),
        )
        .await,
        read,
    );
    push(
        "QueryService.Log",
        rpc(t, tok, "QueryService", "Log", json!({ "repo": repo })).await,
        read,
    );
    push(
        "RefService.ListRefs",
        rpc(t, tok, "RefService", "ListRefs", json!({ "repo": repo })).await,
        read,
    );
    push(
        "GitService.GitListRefs",
        rpc(t, tok, "GitService", "GitListRefs", json!({ "repo": repo })).await,
        read,
    );
    push(
        "git upload-pack",
        git_info_refs(t, tok, &name, false).await,
        http_class(read),
    );
    push(
        "LFS batch download",
        lfs_batch(t, tok, &name, false).await,
        http_class(read),
    );
    let check = t
        .call_as(
            tok.unwrap_or(""),
            "AuthService",
            "CheckAccess",
            json!({ "repo": repo, "action": "read", "paths": [b64("README.md")] }),
        )
        .await;
    let check = match check {
        Ok(v) if v["allowed"][0] == true => "OK".to_string(),
        Ok(_) => "POLICY_DENIED".to_string(),
        Err(e) => e.reason,
    };
    push("AuthService.CheckAccess", check, read);
    let obj = t
        .call(
            "AuthService",
            "CheckObjectAccess",
            json!({ "repo": repo, "subjectToken": tok.unwrap_or(""), "ids": [id_json(&tip)] }),
        )
        .await;
    let obj = match obj {
        Ok(v) if v["allowed"][0] == true => "OK".to_string(),
        Ok(_) => "POLICY_DENIED".to_string(),
        Err(e) => e.reason,
    };
    push("AuthService.CheckObjectAccess", obj, read);
    let search = t
        .call_as(
            tok.unwrap_or(""),
            "SearchService",
            "SearchRepos",
            json!({ "query": vis.repo() }),
        )
        .await;
    let search = match search {
        Ok(v) => {
            let listed = v["hits"]
                .as_array()
                .is_some_and(|h| h.iter().any(|h| h["fullName"] == name.as_str()));
            if listed { "OK" } else { "HIDDEN" }.to_string()
        }
        Err(e) => e.reason,
    };
    let want_search = match read {
        "OK" => "OK",
        "UNAUTHENTICATED" => "UNAUTHENTICATED",
        _ => "HIDDEN",
    };
    push("SearchService.SearchRepos", search, want_search);
    push(
        "EventService.ListEvents",
        rpc(
            t,
            tok,
            "EventService",
            "ListEvents",
            json!({ "repo": repo }),
        )
        .await,
        read,
    );
    let mcp_got = mcp(t, tok, "query_log", json!({ "repo": repo })).await;
    push("MCP query_log", mcp_got, read);
    let (status, _) = compat(
        t,
        tok,
        "GET",
        &format!("/repos/{name}/contents/README.md"),
        Value::Null,
    )
    .await;
    push("GitHub contents", status_class(status), http_class(read));

    push(
        "WriteService.UploadObjects",
        upload(t, tok, &name, format!("{who:?} wrote {name}").as_bytes()).await,
        write,
    );
    push(
        "WriteService.Land",
        rpc(
            t,
            tok,
            "WriteService",
            "Land",
            json!({ "repo": repo, "ref": "main", "head": id_json(&tip) }),
        )
        .await,
        write,
    );
    push(
        "git receive-pack",
        git_info_refs(t, tok, &name, true).await,
        http_class(write),
    );
    push(
        "LFS batch upload",
        lfs_batch(t, tok, &name, true).await,
        http_class(write),
    );
    out
}

/// The matrix: {public, internal, private} × {owner, org member, team member, outside
/// collaborator, unrelated user, agent token scoped to another repo, anonymous} × every
/// entry point. Anonymous reads public repos on the public read surfaces only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn visibility_and_roles_decide_access_on_every_surface() {
    let w = world().await;
    let mut checked = 0;
    let mut wrong = Vec::new();
    for vis in [Vis::Public, Vis::Internal, Vis::Private] {
        for who in WHO {
            for (surface, got, want) in probe_cell(&w, vis, who).await {
                checked += 1;
                if got != want {
                    wrong.push(format!("{vis:?} {who:?} {surface}: got {got}, want {want}"));
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {checked} wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    assert_eq!(checked, 3 * 7 * 16);
}

/// Archive: reads keep working, every write surface answers `ARCHIVED`; unarchive brings
/// writes back. Delete: everything is `NOT_FOUND` (to the owner too), signed URLs issued
/// before stop working, search forgets the repo; restore brings it all back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_delete_and_restore_apply_on_every_surface() {
    let t = start_default().await;
    let created = create_repo(&t, "life", "VISIBILITY_PRIVATE").await;
    let repo_id = created["id"].as_str().unwrap().to_string();
    let name = "dev/life";
    let repo = rref(name);
    // A large blob, landed, for signed URLs.
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 249) as u8).collect();
    let tip = tip_of(&t, name).await;
    let ids = {
        let id = bee_object::blob_id(&big).to_text();
        let m = json!({ "repo": repo, "id": id_json(&id), "offset": "0", "data": B64.encode(&big), "last": true });
        t.call("WriteService", "UploadObjects", json!({ "messages": [m] }))
            .await
            .expect("upload");
        vec![id]
    };
    let cs = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": repo,
                "parents": [id_json(&tip)],
                "delta": [{ "path": b64("big.bin"), "put": { "mode": "MODE_FILE", "blob": id_json(&ids[0]), "size": big.len().to_string() } }],
                "changeId": change_id(4401),
                "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": "big",
            }),
        )
        .await
        .expect("changeset");
    let head = id_text(&cs["changeset"]["id"]);
    t.call(
        "WriteService",
        "Land",
        json!({ "repo": repo, "ref": "main", "head": id_json(&head) }),
    )
    .await
    .expect("land");
    let signed = || {
        let id = ids[0].clone();
        let repo = repo.clone();
        let tok = t.token.clone();
        let t = &t;
        async move {
            let r = t
                .call_as(
                    &tok,
                    "ObjectService",
                    "GetObjects",
                    json!({ "repo": repo, "ids": [id_json(&id)], "allowSignedUrls": true }),
                )
                .await
                .expect("get objects");
            r["items"][0]["signedUrl"].as_str().unwrap().to_string()
        }
    };
    let url = signed().await;
    assert!(url.contains("&g=") && url.contains("&gs="), "{url}");
    let (s, _, bytes) = http("GET", &url, &[], Vec::new()).await;
    assert_eq!(s, 200);
    assert_eq!(bytes, big);
    // A URL whose grant was stripped is refused.
    let stripped = url.split("&g=").next().unwrap().to_string();
    let (s, _, _) = http("GET", &stripped, &[], Vec::new()).await;
    assert_eq!(s, 401, "a URL without its grant");
    let import_src = t.dir.path().join("import-src");
    std::fs::create_dir_all(&import_src).unwrap();
    let import = || {
        let src = import_src.clone();
        let tok = t.token.clone();
        let t = &t;
        async move {
            norm(
                t.call_as(
                    &tok,
                    "ImportService",
                    "StartImport",
                    json!({ "sourceKind": "IMPORT_SOURCE_KIND_GIT", "source": "dev/life", "sourceUrl": format!("file://{}", src.display()) }),
                )
                .await,
            )
        }
    };

    // ----- archive -----
    t.call("RepoService", "ArchiveRepo", json!({ "repo": repo }))
        .await
        .expect("archive");
    let tok = Some(t.token.as_str());
    for (what, got) in [
        (
            "ResolveRef",
            rpc(
                &t,
                tok,
                "ObjectService",
                "ResolveRef",
                json!({ "repo": repo, "name": "main" }),
            )
            .await,
        ),
        (
            "Log",
            rpc(&t, tok, "QueryService", "Log", json!({ "repo": repo })).await,
        ),
        ("git upload-pack", git_info_refs(&t, tok, name, false).await),
        ("LFS download", lfs_batch(&t, tok, name, false).await),
    ] {
        assert_eq!(got, "OK", "archived repos still read: {what}");
    }
    let (s, _, _) = http("GET", &signed().await, &[], Vec::new()).await;
    assert_eq!(s, 200, "signed URLs of archived repos serve");
    let (s, _) = compat(
        &t,
        tok,
        "GET",
        "/repos/dev/life/contents/README.md",
        Value::Null,
    )
    .await;
    assert_eq!(s, 200);
    let rebase = rpc(
        &t,
        tok,
        "WriteService",
        "Rebase",
        json!({ "repo": repo, "changeset": id_json(&head), "onto": { "expr": "main" } }),
    )
    .await;
    for (what, got) in [
        (
            "UploadObjects",
            upload(&t, tok, name, b"after archive").await,
        ),
        (
            "Land",
            rpc(
                &t,
                tok,
                "WriteService",
                "Land",
                json!({ "repo": repo, "ref": "main", "head": id_json(&head) }),
            )
            .await,
        ),
        (
            "CreateRef",
            rpc(
                &t,
                tok,
                "RefService",
                "CreateRef",
                json!({ "repo": repo, "name": "feature", "target": id_json(&head) }),
            )
            .await,
        ),
        ("Rebase", rebase),
        ("git receive-pack", git_info_refs(&t, tok, name, true).await),
        ("LFS upload", lfs_batch(&t, tok, name, true).await),
        ("import into it", import().await),
    ] {
        assert_eq!(got, "ARCHIVED", "{what} on an archived repo");
    }
    let (s, body) = compat(
        &t,
        tok,
        "POST",
        "/repos/dev/life/git/blobs",
        json!({ "content": B64.encode(b"x"), "encoding": "base64" }),
    )
    .await;
    assert_eq!(s, 403, "{body}");
    assert!(
        body.to_string().to_lowercase().contains("archived"),
        "{body}"
    );
    t.call("RepoService", "UnarchiveRepo", json!({ "repo": repo }))
        .await
        .expect("unarchive");
    assert_eq!(upload(&t, tok, name, b"after unarchive").await, "OK");

    // ----- delete -----
    let before_delete = signed().await;
    t.call(
        "RepoService",
        "DeleteRepo",
        json!({ "repo": repo, "confirmFullName": name }),
    )
    .await
    .expect("delete");
    for (what, got) in [
        (
            "ResolveRef",
            rpc(
                &t,
                tok,
                "ObjectService",
                "ResolveRef",
                json!({ "repo": repo, "name": "main" }),
            )
            .await,
        ),
        (
            "ResolveRef by id",
            rpc(
                &t,
                tok,
                "ObjectService",
                "ResolveRef",
                json!({ "repo": { "id": repo_id }, "name": "main" }),
            )
            .await,
        ),
        (
            "Log",
            rpc(&t, tok, "QueryService", "Log", json!({ "repo": repo })).await,
        ),
        (
            "UploadObjects",
            upload(&t, tok, name, b"after delete").await,
        ),
        (
            "CheckAccess",
            rpc(
                &t,
                tok,
                "AuthService",
                "CheckAccess",
                json!({ "repo": repo, "action": "read", "paths": [] }),
            )
            .await,
        ),
        ("import into it", import().await),
    ] {
        assert_eq!(got, "NOT_FOUND", "{what} on a deleted repo");
    }
    assert_eq!(git_info_refs(&t, tok, name, false).await, "404");
    assert_eq!(lfs_batch(&t, tok, name, false).await, "404");
    let (s, _, _) = http("GET", &before_delete, &[], Vec::new()).await;
    assert_eq!(s, 404, "a signed URL issued before the delete");
    let (s, _) = compat(
        &t,
        tok,
        "GET",
        "/repos/dev/life/contents/README.md",
        Value::Null,
    )
    .await;
    assert_eq!(s, 404);
    let r = t
        .call("SearchService", "SearchRepos", json!({ "query": "life" }))
        .await
        .expect("search");
    assert!(
        !r["hits"]
            .as_array()
            .is_some_and(|h| h.iter().any(|h| h["fullName"] == name)),
        "{r}"
    );

    // ----- restore -----
    t.call("RepoService", "RestoreRepo", json!({ "repoId": repo_id }))
        .await
        .expect("restore");
    assert_eq!(tip_of(&t, name).await, head);
    assert_eq!(git_info_refs(&t, tok, name, false).await, "OK");
    let (s, _, bytes) = http("GET", &signed().await, &[], Vec::new()).await;
    assert_eq!(s, 200);
    assert_eq!(bytes, big);
}

/// Transfer: the repo answers under the new owner's name (and the old one redirects), access
/// follows the new owner at once, token repo patterns match the new name, and every object
/// stays addressable by its id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfer_moves_access_and_keeps_objects_addressable() {
    let t = start_default().await;
    create_repo(&t, "xfer", "VISIBILITY_PRIVATE").await;
    let old_tip = tip_of(&t, "dev/xfer").await;
    let (member_id, member) = t.principal("human", "devmember", "").await;
    join_dev(&t, &member_id).await;
    let (carol_id, carol) = t.principal("human", "carol", "").await;
    t.call_as(
        &carol,
        "OrgService",
        "CreateOrg",
        json!({ "name": "acme", "display": "Acme" }),
    )
    .await
    .expect("acme");
    t.call_as(
        &carol,
        "OrgService",
        "SetMember",
        json!({ "org": "acme", "principalId": t.dev_user(), "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("dev joins acme");
    let resolve = |tok: String, name: &'static str| {
        let t = &t;
        async move {
            rpc(
                t,
                Some(&tok),
                "ObjectService",
                "ResolveRef",
                json!({ "repo": rref(name), "name": "main" }),
            )
            .await
        }
    };
    assert_eq!(resolve(member.clone(), "dev/xfer").await, "OK");
    assert_eq!(resolve(carol.clone(), "dev/xfer").await, "NOT_FOUND");
    let moved = t
        .call(
            "RepoService",
            "TransferRepo",
            json!({ "repo": rref("dev/xfer"), "newOrg": "acme" }),
        )
        .await
        .expect("transfer");
    assert_eq!(moved["fullName"], "acme/xfer");
    // At once, with no wait: the new owner reads under both names, the old org's member no
    // longer does.
    assert_eq!(resolve(carol.clone(), "acme/xfer").await, "OK");
    assert_eq!(
        resolve(carol.clone(), "dev/xfer").await,
        "OK",
        "old name redirects"
    );
    assert_eq!(resolve(member.clone(), "acme/xfer").await, "NOT_FOUND");
    assert_eq!(resolve(member.clone(), "dev/xfer").await, "NOT_FOUND");
    // The previous owner is now an acme member with the base role (read): no more writes.
    assert_eq!(resolve(t.token.clone(), "acme/xfer").await, "OK");
    assert_eq!(
        upload(&t, Some(&t.token), "acme/xfer", b"after transfer").await,
        "PERMISSION_DENIED"
    );
    assert_eq!(
        upload(&t, Some(&carol), "acme/xfer", b"carol writes").await,
        "OK"
    );
    // Objects keep their ids.
    let r = t
        .call_as(
            &carol,
            "QueryService",
            "GetChangesets",
            json!({ "repo": rref("acme/xfer"), "ids": [id_json(&old_tip)] }),
        )
        .await
        .expect("old changeset by id");
    assert!(!r.to_string().contains("NOT_FOUND"), "{r}");
    assert_eq!(tip_of(&t, "acme/xfer").await, old_tip);
    // Git answers under the new name.
    assert_eq!(
        git_info_refs(&t, Some(&carol), "acme/xfer", false).await,
        "OK"
    );
    // Token repo patterns match the repo's current name.
    let new_scoped = scoped_agent_token(&t, &["acme/xfer"]);
    assert_eq!(resolve(new_scoped, "acme/xfer").await, "OK");
    let old_scoped = scoped_agent_token(&t, &["dev/xfer"]);
    assert_eq!(resolve(old_scoped, "acme/xfer").await, "POLICY_DENIED");
    let _ = carol_id;
}

/// A change through this server applies to its very next request; a change made beside it
/// (straight in the forge store, as another process would) is seen within
/// `forge.access_cache_ms`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_changes_reach_cached_decisions_within_the_bound() {
    let t = start_default().await;
    let created = create_repo(&t, "vis", "VISIBILITY_PRIVATE").await;
    let repo_id = created["id"].as_str().unwrap().to_string();
    let (_, stranger) = t.principal("human", "stranger", "").await;
    let (member_id, member) = t.principal("human", "member", "").await;
    join_dev(&t, &member_id).await;
    let read = |tok: String| {
        let t = &t;
        async move {
            rpc(
                t,
                Some(&tok),
                "QueryService",
                "Log",
                json!({ "repo": rref("dev/vis") }),
            )
            .await
        }
    };
    assert_eq!(read(stranger.clone()).await, "NOT_FOUND");
    assert_eq!(read(member.clone()).await, "OK");
    t.call(
        "RepoService",
        "UpdateRepo",
        json!({ "repo": rref("dev/vis"), "visibility": "VISIBILITY_PUBLIC" }),
    )
    .await
    .expect("public");
    assert_eq!(read(stranger.clone()).await, "OK", "made public: at once");
    t.call(
        "RepoService",
        "UpdateRepo",
        json!({ "repo": rref("dev/vis"), "visibility": "VISIBILITY_PRIVATE" }),
    )
    .await
    .expect("private");
    assert_eq!(
        read(stranger.clone()).await,
        "NOT_FOUND",
        "made private: at once"
    );
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": member_id, "role": "ROLE_UNSPECIFIED" }),
    )
    .await
    .expect("member removed");
    assert_eq!(
        read(member.clone()).await,
        "NOT_FOUND",
        "removed member: at once"
    );

    // Beside the server: the forge store changes without passing this server's request path.
    assert_eq!(read(t.token.clone()).await, "OK");
    let w = t.srv.forge.as_ref().expect("forge");
    let started = Instant::now();
    w.forge
        .orgs
        .update_repo(
            &bee_forge::Cx::system(),
            &repo_id,
            bee_forge::orgs::repos::RepoPatch {
                visibility: Some(bee_proto::v1::Visibility::Public),
                ..Default::default()
            },
            0,
        )
        .await
        .expect("forge update");
    loop {
        if read(stranger.clone()).await == "OK" {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "a stale deny outlived forge.access_cache_ms"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started = Instant::now();
    w.forge
        .orgs
        .update_repo(
            &bee_forge::Cx::system(),
            &repo_id,
            bee_forge::orgs::repos::RepoPatch {
                visibility: Some(bee_proto::v1::Visibility::Private),
                ..Default::default()
            },
            0,
        )
        .await
        .expect("forge update");
    loop {
        if read(stranger.clone()).await == "NOT_FOUND" {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "a stale allow outlived forge.access_cache_ms"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Private forks (`docs/spec/forks.md`): readable by the fork org's members and by those
/// holding maintain or admin on the parent, by nobody else; the fork opens nothing of the
/// parent to its own org.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_fork_visibility_holds_on_the_vcs_path() {
    let t = start_default().await;
    t.call(
        "OrgService",
        "UpdateOrgSettings",
        json!({ "org": "dev", "baseRepoRole": "REPO_ROLE_NONE" }),
    )
    .await
    .expect("base role none");
    create_repo(&t, "fsrc", "VISIBILITY_PRIVATE").await;
    let (carol_id, carol) = t.principal("human", "carol", "").await;
    t.call_as(
        &carol,
        "OrgService",
        "CreateOrg",
        json!({ "name": "acme", "display": "Acme" }),
    )
    .await
    .expect("acme");
    t.call_as(
        &carol,
        "OrgService",
        "SetMember",
        json!({ "org": "acme", "principalId": t.dev_user(), "role": "ROLE_MEMBER" }),
    )
    .await
    .expect("dev joins acme");
    let (maint_id, maint) = t.principal("human", "maint", "").await;
    collaborate(&t, "dev/fsrc", &maint_id, &maint, "REPO_ROLE_MAINTAIN").await;
    let (writer_id, writer) = t.principal("human", "writer", "").await;
    collaborate(&t, "dev/fsrc", &writer_id, &writer, "REPO_ROLE_WRITE").await;
    let fork = t
        .call(
            "RepoService",
            "ForkRepo",
            json!({ "repo": rref("dev/fsrc"), "org": "acme" }),
        )
        .await
        .expect("fork");
    assert_eq!(fork["fullName"], "acme/fsrc");
    let read = |tok: String, name: &'static str| {
        let t = &t;
        async move {
            rpc(
                t,
                Some(&tok),
                "QueryService",
                "Log",
                json!({ "repo": rref(name) }),
            )
            .await
        }
    };
    assert_eq!(
        read(carol.clone(), "acme/fsrc").await,
        "OK",
        "the fork org's owner"
    );
    assert_eq!(
        read(t.token.clone(), "acme/fsrc").await,
        "OK",
        "a fork org member"
    );
    assert_eq!(
        read(maint.clone(), "acme/fsrc").await,
        "OK",
        "maintain on the parent"
    );
    assert_eq!(
        read(writer.clone(), "acme/fsrc").await,
        "NOT_FOUND",
        "write on the parent"
    );
    assert_eq!(
        read(carol.clone(), "dev/fsrc").await,
        "NOT_FOUND",
        "the parent stays closed"
    );
    assert_eq!(
        git_info_refs(&t, Some(&writer), "acme/fsrc", false).await,
        "404"
    );
    assert_eq!(
        git_info_refs(&t, Some(&maint), "acme/fsrc", false).await,
        "OK"
    );
    let _ = carol_id;
}

/// An open event subscription ends with the refusal once its subscriber loses read access
/// (within the forge's re-check interval), instead of streaming on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_event_streams_end_when_read_access_goes() {
    use bee_proto::v1;
    let t = start_default().await;
    t.call(
        "OrgService",
        "UpdateOrgSettings",
        json!({ "org": "dev", "baseRepoRole": "REPO_ROLE_READ" }),
    )
    .await
    .expect("base role read");
    create_repo(&t, "events", "VISIBILITY_PRIVATE").await;
    let (member_id, member) = t.principal("human", "member", "").await;
    join_dev(&t, &member_id).await;
    let mut client = v1::event_service_client::EventServiceClient::new(t.grpc_channel().await);
    let mut stream = client
        .subscribe(t.bearer(
            &member,
            v1::SubscribeRequest {
                repo: Some(v1::RepoRef {
                    repo: Some(v1::repo_ref::Repo::FullName("dev/events".into())),
                }),
                ..Default::default()
            },
        ))
        .await
        .expect("subscribe")
        .into_inner();
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": member_id, "role": "ROLE_UNSPECIFIED" }),
    )
    .await
    .expect("member removed");
    let started = Instant::now();
    let refusal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.message().await {
                Ok(Some(_)) => continue,
                Ok(None) => return None,
                Err(s) => return Some(detail(&s).reason),
            }
        }
    })
    .await
    .expect("the stream ends");
    assert_eq!(refusal.as_deref(), Some("NOT_FOUND"));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn lfs_href(t: &TestServer, token: &str, op: &str, oid: &str, size: usize) -> String {
    let url = format!("{}/dev/lfs.git/info/lfs/objects/batch", t.url());
    let auth = basic(token);
    let h: Vec<(&str, &str)> = vec![
        ("accept", "application/vnd.git-lfs+json"),
        ("content-type", "application/vnd.git-lfs+json"),
        ("authorization", auth.as_str()),
    ];
    let body = json!({ "operation": op, "transfers": ["basic"], "objects": [{ "oid": oid, "size": size }] });
    let (status, _, out) = http("POST", &url, &h, body.to_string().into_bytes()).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&out));
    let v: Value = serde_json::from_slice(&out).unwrap();
    v["objects"][0]["actions"][op]["href"]
        .as_str()
        .unwrap_or_else(|| panic!("no {op} href: {v}"))
        .to_string()
}

/// LFS transfer URLs are decided again when used: an upload URL stops working once the repo
/// is archived, a download URL once its principal loses read access.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_transfer_urls_are_reauthorized_when_redeemed() {
    let t = start_default().await;
    create_repo(&t, "lfs", "VISIBILITY_PRIVATE").await;
    let (member_id, member) = t.principal("human", "member", "").await;
    join_dev(&t, &member_id).await;
    let data = b"large file stand-in\n".to_vec();
    let oid = sha256_hex(&data);
    let put = lfs_href(&t, &t.token, "upload", &oid, data.len()).await;
    let (s, _, _) = http("PUT", &put, &[], data.clone()).await;
    assert_eq!(s, 200, "upload while writable");
    let get = lfs_href(&t, &member, "download", &oid, data.len()).await;
    let (s, _, body) = http("GET", &get, &[], Vec::new()).await;
    assert_eq!((s, body), (200, data.clone()));

    // An upload URL issued before the archive is refused after it.
    let other = b"second object\n".to_vec();
    let put2 = lfs_href(&t, &t.token, "upload", &sha256_hex(&other), other.len()).await;
    t.call(
        "RepoService",
        "ArchiveRepo",
        json!({ "repo": rref("dev/lfs") }),
    )
    .await
    .expect("archive");
    let (s, _, body) = http("PUT", &put2, &[], other).await;
    assert_eq!(s, 400, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("ARCHIVED"));
    // Downloads still serve on an archived repo ...
    let (s, _, _) = http("GET", &get, &[], Vec::new()).await;
    assert_eq!(s, 200);
    // ... until the principal they were issued to loses read access.
    t.call(
        "OrgService",
        "SetMember",
        json!({ "org": "dev", "principalId": member_id, "role": "ROLE_UNSPECIFIED" }),
    )
    .await
    .expect("member removed");
    let (s, _, _) = http("GET", &get, &[], Vec::new()).await;
    assert_eq!(s, 404);
}
