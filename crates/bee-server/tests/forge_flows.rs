//! Forge flows that cross into the VCS: the forge as a separate failure domain, the merge
//! queue landing through the coalescer, `push.autoChange` over git smart HTTP, wiki and gist
//! repos, and the `bee` CLI's `cr` shortcuts against the real server.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use base64::Engine;
use bee_forge::store::ForgeStore;
use bee_forge::store::unavailable::{ALL_SURFACES, SwitchableStore};
use bee_server::server::{ServerOptions, start};
use common::*;
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_vcs_reads_writes_and_lands_while_the_forge_database_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(dir.path());
    let sql = bee_forge::store::sql::SqlStore::open(
        bee_forge::store::sql::SqlConfig::sqlite(dir.path().join("forge")),
        Arc::new(bee_forge::ids::SystemClock),
    )
    .await
    .unwrap();
    let switch = Arc::new(SwitchableStore::new(Arc::new(sql)));
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = true;
    opts.forge_store = Some(switch.clone() as Arc<dyn ForgeStore>);
    let srv = start(opts).await.unwrap();
    let token = srv.dev.as_ref().unwrap().token.clone();
    let t = TestServer {
        srv,
        token,
        dir,
        cfg,
    };
    assert!(t.srv.forge.is_some(), "the forge runs");
    t.call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect("forge up");
    // Repo access is decided with the forge (SPEC §9.6): the dev user's decision is known
    // before the outage, a principal never seen before has none. (The bootstrap's forge
    // events drop cached decisions as they are relayed, so warm the cache after them.)
    eventually(30, "the bootstrap events relayed", || async {
        let ev = t
            .call("EventService", "ListEvents", json!({ "repo": repo() }))
            .await
            .ok()?;
        ev["events"]
            .as_array()?
            .iter()
            .any(|e| e["kind"] == "collaborator.added")
            .then_some(())
    })
    .await;
    t.tip("main").await;
    let (_, stranger) = t.principal("human", "stranger", "").await;

    switch.take_down(bee_forge::store::Surface(ALL_SURFACES));
    // Forge RPCs fail with a retryable reason ...
    let e = t
        .call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect_err("forge down");
    assert_eq!(reason(&e), "UNAVAILABLE");
    assert!(e.retryable);
    // Repo access fails closed for a caller whose access cannot be decided ...
    let e = t
        .call_as(
            &stranger,
            "ObjectService",
            "ResolveRef",
            json!({ "repo": repo(), "name": "main" }),
        )
        .await
        .expect_err("undecidable access");
    assert_eq!(reason(&e), "UNAVAILABLE");
    assert!(e.retryable);
    // ... while the VCS keeps working for known callers: write, land on the protected ref, read.
    let (tip, v) = t.tip("main").await;
    let c = id_text(
        &t.commit(&tip, &[("outage.txt", b"still here\n")], 900, "outage")
            .await["id"],
    );
    let landed = t.land(&c).await.expect("land during the forge outage");
    assert_eq!(landed["ref"]["version"], (v + 1).to_string());
    let (tip2, _) = t.tip("main").await;
    assert_eq!(t.read("outage.txt", &tip2).await.unwrap(), b"still here\n");
    t.call("QueryService", "Log", json!({ "repo": repo() }))
        .await
        .expect("history read");

    // Recovery: the forge answers again and the land made during the outage reaches the
    // repo's event log.
    switch.bring_up(bee_forge::store::Surface(ALL_SURFACES));
    t.call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect("forge back");
    let tip_text = tip2.clone();
    eventually(30, "the outage land in the forge log", || async {
        let ev = t
            .call(
                "EventService",
                "ListEvents",
                json!({ "repo": repo(), "kinds": ["ref.updated"] }),
            )
            .await
            .ok()?;
        let target = id_json(&tip_text);
        ev["events"]
            .as_array()?
            .iter()
            .any(|e| e["refUpdated"]["target"] == target)
            .then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remembered_queue_requirement_keeps_a_branch_gated_while_the_forge_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(dir.path());
    let sql = bee_forge::store::sql::SqlStore::open(
        bee_forge::store::sql::SqlConfig::sqlite(dir.path().join("forge")),
        Arc::new(bee_forge::ids::SystemClock),
    )
    .await
    .unwrap();
    let switch = Arc::new(SwitchableStore::new(Arc::new(sql)));
    let mut opts = ServerOptions::new(cfg.clone());
    opts.dev = true;
    opts.forge_store = Some(switch.clone() as Arc<dyn ForgeStore>);
    let srv = start(opts).await.unwrap();
    let token = srv.dev.as_ref().unwrap().token.clone();
    let t = TestServer {
        srv,
        token,
        dir,
        cfg,
    };

    t.call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect("default forge access is available");
    eventually(
        30,
        "the default repository access event is relayed",
        || async {
            let events = t
                .call("EventService", "ListEvents", json!({ "repo": repo() }))
                .await
                .ok()?;
            events["events"]
                .as_array()?
                .iter()
                .any(|event| event["kind"] == "collaborator.added")
                .then_some(())
        },
    )
    .await;

    t.call(
        "RepoService",
        "UpdateRepoSettings",
        json!({ "repo": repo(), "mergeQueueRequired": true }),
    )
    .await
    .expect("require the queue on the default repository");
    let (required_tip, required_version) = t.tip("main").await;
    let required_head = id_text(
        &t.commit(
            &required_tip,
            &[("outage/queued.txt", b"must use queue\n")],
            960,
            "queue-required outage",
        )
        .await["id"],
    );
    let error = t
        .land(&required_head)
        .await
        .expect_err("direct land is refused while the forge is available");
    assert_eq!(reason(&error), "INVALID_STATE", "{error:?}");
    assert_eq!(error.subcode, "merge_queue_required", "{error:?}");
    assert_eq!(
        t.tip("main").await,
        (required_tip.clone(), required_version),
        "the rejected direct land leaves main unchanged"
    );

    // Prepare a second repository without the setting and warm its access decision before
    // the outage, matching the known-caller setup in the neighboring outage test.
    let ordinary_repo = json!({ "fullName": "dev/outage-ordinary" });
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": "outage-ordinary", "autoInit": true }),
    )
    .await
    .expect("create ordinary repository");
    eventually(
        30,
        "the ordinary repository creation event is relayed",
        || async {
            let events = t
                .call(
                    "EventService",
                    "ListEvents",
                    json!({ "repo": ordinary_repo }),
                )
                .await
                .ok()?;
            events["events"]
                .as_array()?
                .iter()
                .any(|event| {
                    event["kind"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("repo.")
                })
                .then_some(())
        },
    )
    .await;
    t.call(
        "IssueService",
        "ListIssues",
        json!({ "repo": ordinary_repo }),
    )
    .await
    .expect("ordinary forge access is available");
    t.call("IssueService", "ListIssues", json!({ "repo": repo() }))
        .await
        .expect("default forge access remains available");
    t.tip("main").await;
    let ordinary_tip_value = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": ordinary_repo, "name": "main" }),
        )
        .await
        .expect("ordinary ref resolves");
    let ordinary_tip = id_text(&ordinary_tip_value["ref"]["target"]);
    let ordinary_blob = b"ordinary branch still lands\n";
    let ordinary_blob_id = bee_object::blob_id(ordinary_blob).to_text();
    t.call(
        "WriteService",
        "UploadObjects",
        json!({ "messages": [{
            "repo": ordinary_repo,
            "id": id_json(&ordinary_blob_id),
            "offset": "0",
            "data": B64.encode(ordinary_blob),
            "last": true
        }] }),
    )
    .await
    .expect("upload ordinary change");
    let ordinary_change = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({
                "repo": ordinary_repo,
                "parents": [id_json(&ordinary_tip)],
                "delta": [{ "path": B64.encode(b"outage.txt"), "put": {
                    "mode": "MODE_FILE", "blob": id_json(&ordinary_blob_id), "size": ordinary_blob.len().to_string()
                }}],
                "changeId": change_id(961),
                "author": "tester",
                "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                "message": "ordinary outage land"
            }),
        )
        .await
        .expect("create ordinary changeset");
    let ordinary_head = ordinary_change["changeset"]["id"].clone();

    switch.take_down(bee_forge::store::Surface(ALL_SURFACES));

    // The remembered setting is true, but require_landing_queue_entry then needs the forge
    // repository row to confirm a durable queue entry; while that lookup is down, the code
    // returns retryable UNAVAILABLE instead of allowing a direct land.
    let error = t
        .land(&required_head)
        .await
        .expect_err("remembered queue requirement keeps direct land gated");
    assert_eq!(reason(&error), "UNAVAILABLE", "{error:?}");
    assert!(error.retryable, "{error:?}");
    assert_eq!(
        t.tip("main").await,
        (required_tip.clone(), required_version),
        "main stays unmoved during the outage"
    );

    t.call(
        "WriteService",
        "Land",
        json!({ "repo": ordinary_repo, "ref": "main", "head": ordinary_head }),
    )
    .await
    .expect("ordinary repository lands during the forge outage");
    let ordinary_after = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": ordinary_repo, "name": "main" }),
        )
        .await
        .expect("ordinary ref remains readable");
    assert_eq!(
        id_text(&ordinary_after["ref"]["target"]),
        id_text(&ordinary_head),
        "ordinary main advances during the outage"
    );

    switch.bring_up(bee_forge::store::Surface(ALL_SURFACES));
    let error = t
        .land(&required_head)
        .await
        .expect_err("direct land remains gated after recovery");
    assert_eq!(reason(&error), "INVALID_STATE", "{error:?}");
    assert_eq!(error.subcode, "merge_queue_required", "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_merge_queue_lands_queued_changes_in_order() {
    let t = start_default().await;
    t.call(
        "RepoService",
        "UpdateRepoSettings",
        json!({ "repo": repo(), "mergeQueueRequired": true }),
    )
    .await
    .expect("queue required");
    let (tip, v0) = t.tip("main").await;
    let mut ids = Vec::new();
    for (i, f) in ["q1.txt", "q2.txt"].iter().enumerate() {
        let head = id_text(&t.commit(&tip, &[(f, b"q\n")], 700 + i as u64, f).await["id"]);
        let cr = t
            .call(
                "ChangeService",
                "OpenChange",
                json!({ "repo": repo(), "head": id_json(&head), "title": f }),
            )
            .await
            .unwrap();
        ids.push(cr["id"].as_str().unwrap().to_string());
    }
    let e = t
        .call(
            "ChangeService",
            "LandChange",
            json!({ "repo": repo(), "id": ids[0] }),
        )
        .await
        .expect_err("direct land refused");
    assert_eq!(e.subcode, "merge_queue_required");
    for id in &ids {
        t.call(
            "ChangeService",
            "EnqueueChange",
            json!({ "repo": repo(), "id": id }),
        )
        .await
        .expect("enqueue");
    }
    for id in &ids {
        eventually(30, "queued change to land", || async {
            let c = t
                .call(
                    "ChangeService",
                    "GetChange",
                    json!({ "repo": repo(), "id": id }),
                )
                .await
                .ok()?;
            (c["state"] == "CHANGE_STATE_LANDED").then_some(())
        })
        .await;
    }
    let (tip2, v2) = t.tip("main").await;
    assert_eq!(v2, v0 + 2);
    assert_eq!(t.read("q1.txt", &tip2).await.unwrap(), b"q\n");
    assert_eq!(t.read("q2.txt", &tip2).await.unwrap(), b"q\n");
    let q = t
        .call("ChangeService", "GetMergeQueue", json!({ "repo": repo() }))
        .await
        .unwrap();
    assert!(q["entries"].as_array().is_none_or(|e| e.is_empty()), "{q}");
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn encode(token: &str) -> String {
    token
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_git_push_with_bee_cr_opens_and_revises_a_change_request() {
    let t = start_default().await;
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        "[user]\n\tname = Tester\n\temail = tester@example.com\n[init]\n\tdefaultBranch = main\n[protocol]\n\tversion = 2\n",
    )
    .unwrap();
    let url = format!(
        "http://git:{}@{}/dev/playground.git",
        encode(&t.token),
        t.srv.addr
    );
    let work = home.path().join("w");
    let h = home.path().to_path_buf();
    let (u, w) = (url.clone(), work.clone());
    tokio::task::spawn_blocking(move || {
        git(&h, &h, &["clone", "-q", &u, w.to_str().unwrap()]);
        git(&h, &w, &["checkout", "-q", "-b", "feature/login"]);
        std::fs::write(w.join("login.txt"), "v1\n").unwrap();
        git(&h, &w, &["add", "login.txt"]);
        git(&h, &w, &["commit", "-q", "-m", "Add login"]);
        git(
            &h,
            &w,
            &[
                "push",
                "-q",
                "-o",
                "bee.cr",
                "-o",
                "bee.cr.title=Login page",
                "origin",
                "feature/login",
            ],
        );
    })
    .await
    .unwrap();
    let cr = eventually(30, "the pushed CR", || async {
        let l = t
            .call("ChangeService", "ListChanges", json!({ "repo": repo() }))
            .await
            .ok()?;
        l["changes"]
            .as_array()?
            .iter()
            .find(|c| c["title"] == "Login page")
            .cloned()
    })
    .await;
    assert_eq!(cr["targetRef"], "main");
    assert_eq!(cr["revisions"].as_array().unwrap().len(), 1);
    let id = cr["id"].as_str().unwrap().to_string();

    // A second push to the branch is a new revision of the same CR.
    let h = home.path().to_path_buf();
    let w = work.clone();
    tokio::task::spawn_blocking(move || {
        std::fs::write(w.join("login.txt"), "v2\n").unwrap();
        git(&h, &w, &["commit", "-q", "-am", "Polish login"]);
        git(&h, &w, &["push", "-q", "origin", "feature/login"]);
    })
    .await
    .unwrap();
    eventually(30, "the second revision", || async {
        let c = t
            .call(
                "ChangeService",
                "GetChange",
                json!({ "repo": repo(), "id": id }),
            )
            .await
            .ok()?;
        (c["revisions"].as_array()?.len() == 2).then_some(())
    })
    .await;
    // A push without the option and without push.autoChange opens nothing.
    let h = home.path().to_path_buf();
    let w = work.clone();
    tokio::task::spawn_blocking(move || {
        git(&h, &w, &["checkout", "-q", "-b", "scratch"]);
        std::fs::write(w.join("scratch.txt"), "s\n").unwrap();
        git(&h, &w, &["add", "scratch.txt"]);
        git(&h, &w, &["commit", "-q", "-m", "scratch"]);
        git(&h, &w, &["push", "-q", "origin", "scratch"]);
    })
    .await
    .unwrap();
    let l = t
        .call("ChangeService", "ListChanges", json!({ "repo": repo() }))
        .await
        .unwrap();
    assert_eq!(l["changes"].as_array().unwrap().len(), 1, "{l}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wiki_pages_and_gists_live_in_vcs_repos() {
    let t = start_default().await;
    t.call("WikiService", "EnableWiki", json!({ "repo": repo() }))
        .await
        .expect("enable wiki");
    t.call(
        "WikiService",
        "CreateWikiPage",
        json!({ "repo": repo(), "title": "Home", "content": "# Welcome\n" }),
    )
    .await
    .expect("page");
    // The sibling repo `dev/playground.wiki` holds the page as a file.
    let wiki = json!({ "fullName": "dev/playground.wiki" });
    let r = t
        .call(
            "QueryService",
            "ReadPath",
            json!({ "repo": wiki, "at": { "expr": "main" }, "path": B64e(b"Home.md") }),
        )
        .await
        .expect("wiki file in the VCS");
    assert!(r.to_string().contains(&B64e(b"# Welcome\n")), "{r}");
    let gist = t
        .call(
            "GistService",
            "CreateGist",
            json!({ "files": { "hello.rs": "fn main() {}\n" }, "description": "hi", "public": true }),
        )
        .await
        .expect("gist");
    let gid = gist["id"].as_str().unwrap().to_lowercase();
    let r = t
        .call(
            "QueryService",
            "ReadPath",
            json!({
                "repo": { "fullName": format!("-gists/{gid}") },
                "at": { "expr": "main" }, "path": B64e(b"hello.rs"),
            }),
        )
        .await
        .expect("gist file in the VCS");
    assert!(r.to_string().contains(&B64e(b"fn main() {}\n")), "{r}");
}

#[allow(non_snake_case)]
fn B64e(b: &[u8]) -> String {
    use base64::Engine as _;
    B64.encode(b)
}

/// The `bee` binary of this workspace (built on demand).
fn bee_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().and_then(Path::parent).unwrap().to_path_buf();
    let bin = dir.join(format!("bee{}", std::env::consts::EXE_SUFFIX));
    if !bin.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let st = Command::new(cargo)
            .args(["build", "-q", "-p", "bee-cli", "--bin", "bee"])
            .status()
            .expect("cargo build bee");
        assert!(st.success(), "building the bee CLI");
    }
    bin
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bee_cli_cr_shortcuts_work_against_the_server() {
    let t = start_default().await;
    let bin = tokio::task::spawn_blocking(bee_binary).await.unwrap();
    let (tip, _) = t.tip("main").await;
    let head = id_text(
        &t.commit(&tip, &[("cli.txt", b"cli\n")], 800, "cli change")
            .await["id"],
    );
    let cfg_dir = tempfile::tempdir().unwrap();
    let run = |args: Vec<String>| {
        let (bin, url, token, cfg) = (
            bin.clone(),
            t.url().to_string(),
            t.token.clone(),
            cfg_dir.path().to_path_buf(),
        );
        async move {
            tokio::task::spawn_blocking(move || {
                let out = Command::new(bin)
                    .args(&args)
                    .env("BEE_ENDPOINT", url)
                    .env("BEE_TOKEN", token)
                    .env("BEE_CONFIG_DIR", cfg)
                    .stdin(Stdio::null())
                    .output()
                    .expect("bee runs");
                assert!(
                    out.status.success(),
                    "bee {args:?}:\n{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                serde_json::from_slice::<Value>(&out.stdout).expect("json output")
            })
            .await
            .unwrap()
        }
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let opened = run(s(&[
        "cr",
        "open",
        "--repo",
        "dev/playground",
        "--head",
        &head,
        "--title",
        "From the CLI",
        "--json",
    ]))
    .await;
    let id = opened["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(opened["data"]["title"], "From the CLI");
    let listed = run(s(&["cr", "list", "--repo", "dev/playground", "--json"])).await;
    assert!(
        listed["data"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == id.as_str()),
        "{listed}"
    );
    let viewed = run(s(&[
        "cr",
        "view",
        &id,
        "--repo",
        "dev/playground",
        "--json",
    ]))
    .await;
    assert_eq!(viewed["data"]["id"], id.as_str());
    assert_eq!(viewed["data"]["state"], "CHANGE_STATE_OPEN");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_push_protection_blocks_custom_secret_patterns_at_create_changeset() {
    let t = start_default().await;
    t.call(
        "SecurityService",
        "UpdateSecurityConfiguration",
        json!({ "repo": repo(), "secretScanning": true, "secretScanningPushProtection": true }),
    )
    .await
    .expect("security config");
    t.call(
        "SecurityService",
        "CreateSecretScanningPattern",
        json!({
            "repo": repo(), "name": "acme key", "regex": "ACME_KEY_[0-9]{8}",
            "pushProtection": true, "published": true,
        }),
    )
    .await
    .expect("custom pattern");
    let (tip, _) = t.tip("main").await;
    let e = t
        .commit_as(
            &t.token,
            &tip,
            &[("conf.txt", b"key = ACME_KEY_12345678\n")],
            950,
            "leak",
        )
        .await
        .expect_err("blocked by the forge's push protection");
    assert_eq!(reason(&e), "SECRET_DETECTED", "{e:?}");
    t.commit_as(
        &t.token,
        &tip,
        &[("conf.txt", b"key = from-env\n")],
        951,
        "clean",
    )
    .await
    .expect("a clean change passes");
    // CI is served too: listing workflows of a repo without any is empty.
    let w = t
        .call(
            "WorkflowService",
            "ListWorkflows",
            json!({ "repo": repo() }),
        )
        .await
        .expect("workflows");
    assert!(
        w["workflows"].as_array().is_none_or(|a| a.is_empty()),
        "{w}"
    );
}
