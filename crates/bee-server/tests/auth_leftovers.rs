//! Identity and policy leftovers over HTTP/JSON against an in-process `bee-server --dev`:
//! signed commits verified at land (F49-004), enforced required workflows (F49-007), the
//! code-scanning land rule (F49-008), required agent reviews (F49-009), forge teams as
//! policy groups (F49-041) and rulesets scoped by repo custom properties (F416-020).

#![allow(clippy::result_large_err)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use base64::Engine as _;
use common::*;
use serde_json::{Value, json};

fn run(cmd: &str, home: &Path, dir: &Path, args: &[&str]) -> String {
    let out = Command::new(cmd)
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
        .unwrap_or_else(|e| panic!("{cmd} runs: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?}:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
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

async fn resolve(t: &TestServer, name: &str) -> String {
    let r = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": repo(), "name": name }),
        )
        .await
        .unwrap_or_else(|e| panic!("resolve {name}: {e:?}"));
    id_text(&r["ref"]["target"])
}

async fn land_ref(t: &TestServer, head: &str) -> Result<Value, bee_proto::v1::ErrorDetail> {
    t.call(
        "WriteService",
        "Land",
        json!({ "repo": repo(), "ref": "main", "head": id_json(head) }),
    )
    .await
}

/// F49-004: commits signed by real `git` with an SSH key registered on the account verify
/// at land, through VerifyObjectSignature, GetRequiredSignaturesStatus, the forge's land
/// evaluation and the GitHub-compatible commit API; unsigned commits and commits signed by
/// an unregistered key are refused by `require_signed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn require_signed_lands_only_commits_verified_against_registered_keys() {
    if !have("ssh-keygen") {
        eprintln!("skipping: no ssh-keygen");
        return;
    }
    let t = start_default().await;
    let user = t.dev_user();
    let home = tempfile::tempdir().unwrap();
    let h = home.path().to_path_buf();
    for k in ["ok", "stranger"] {
        run(
            "ssh-keygen",
            &h,
            &h,
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                k,
                "-f",
                h.join(k).to_str().unwrap(),
            ],
        );
    }
    let public = std::fs::read_to_string(h.join("ok.pub")).unwrap();
    let key = t
        .call(
            "UserService",
            "AddUserSigningKey",
            json!({ "kind": "USER_SIGNING_KEY_KIND_SSH", "title": "laptop", "publicKey": public }),
        )
        .await
        .expect("register signing key");
    let key_id = key["keyId"].as_str().unwrap().to_string();
    assert!(key_id.starts_with("SHA256:"));

    // The policy takes effect once landed (it is itself landed under the old one).
    t.land_policy("[[land]]\nref = \"main\"\nrequire_signed = true\n", 9100)
        .await;

    std::fs::write(
        h.join(".gitconfig"),
        "[user]\n\tname = Tester\n\temail = tester@example.com\n[init]\n\tdefaultBranch = main\n[gpg]\n\tformat = ssh\n",
    )
    .unwrap();
    let url = format!(
        "http://git:{}@{}/dev/playground.git",
        encode(&t.token),
        t.srv.addr
    );
    let work = h.join("w");
    let (hh, w, u) = (h.clone(), work.clone(), url.clone());
    let shas = tokio::task::spawn_blocking(move || {
        let git = |args: &[&str]| run("git", &hh, &w, args);
        run("git", &hh, &hh, &["clone", "-q", &u, w.to_str().unwrap()]);
        let mut shas = Vec::new();
        for (branch, signer) in [
            ("signed", Some("ok")),
            ("unsigned", None),
            ("foreign", Some("stranger")),
        ] {
            git(&["checkout", "-q", "-B", branch, "origin/main"]);
            std::fs::write(w.join(format!("{branch}.txt")), "x\n").unwrap();
            git(&["add", "."]);
            match signer {
                Some(k) => {
                    let keyfile = format!("user.signingkey={}", hh.join(k).display());
                    git(&["-c", &keyfile, "commit", "-q", "-S", "-m", branch]);
                }
                None => {
                    git(&["commit", "-q", "-m", branch]);
                }
            };
            git(&["push", "-q", "origin", branch]);
            shas.push(git(&["rev-parse", "HEAD"]));
        }
        shas
    })
    .await
    .unwrap();

    let signed = resolve(&t, "signed").await;
    let unsigned = resolve(&t, "unsigned").await;
    let foreign = resolve(&t, "foreign").await;

    // Verification over the API.
    let v = t
        .call(
            "SecurityService",
            "VerifyObjectSignature",
            json!({ "repo": repo(), "changeset": id_json(&signed) }),
        )
        .await
        .expect("verify");
    assert_eq!(v["verified"], true, "{v}");
    assert_eq!(v["reason"], "SECURITY_SIGNATURE_REASON_VALID");
    assert_eq!(v["scheme"], "SECURITY_SIGNATURE_SCHEME_SSH");
    assert_eq!(v["signerKeyId"], key_id);
    assert_eq!(v["signer"]["id"], user);
    let v = t
        .call(
            "SecurityService",
            "VerifyObjectSignature",
            json!({ "repo": repo(), "changeset": id_json(&foreign) }),
        )
        .await
        .unwrap();
    assert_eq!(v["reason"], "SECURITY_SIGNATURE_REASON_UNKNOWN_KEY");
    let status = t
        .call(
            "PolicyService",
            "GetRequiredSignaturesStatus",
            json!({ "repo": repo(), "changesets": [id_json(&signed), id_json(&unsigned), id_json(&foreign)] }),
        )
        .await
        .unwrap();
    assert_eq!(status["required"], true);
    let states: Vec<&str> = status["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["state"].as_str().unwrap())
        .collect();
    assert_eq!(
        states,
        vec![
            "POLICY_SIGNATURE_STATE_VERIFIED",
            "POLICY_SIGNATURE_STATE_UNSIGNED",
            "POLICY_SIGNATURE_STATE_UNKNOWN_KEY"
        ]
    );
    assert_eq!(status["checks"][0]["format"], "ssh");
    assert_eq!(status["checks"][0]["signerPrincipalId"], user);
    assert_eq!(status["checks"][0]["gitOid"], shas[0]);

    // The GitHub-compatible commit carries the verification.
    let auth = format!("token {}", t.token);
    let (code, _, body) = http(
        "GET",
        &format!("{}/repos/dev/playground/commits/{}", t.url(), shas[0]),
        &[
            ("authorization", auth.as_str()),
            ("accept", "application/vnd.github+json"),
        ],
        Vec::new(),
    )
    .await;
    assert_eq!(code, 200);
    let c: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(c["commit"]["verification"]["verified"], true, "{c}");
    assert_eq!(c["commit"]["verification"]["reason"], "valid");
    assert!(
        c["commit"]["verification"]["signature"]
            .as_str()
            .unwrap()
            .contains("BEGIN SSH SIGNATURE")
    );
    let (_, _, body) = http(
        "GET",
        &format!("{}/repos/dev/playground/git/commits/{}", t.url(), shas[1]),
        &[("authorization", auth.as_str())],
        Vec::new(),
    )
    .await;
    let c: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(c["verification"]["reason"], "unsigned", "{c}");

    // Land: unsigned and foreign-signed stacks are refused with the changeset named.
    for head in [&unsigned, &foreign] {
        let e = land_ref(&t, head).await.expect_err("unverified refused");
        assert_eq!(reason(&e), "POLICY_DENIED", "{e:?}");
        assert!(
            e.message.contains(&format!("signed:{head}")),
            "{}",
            e.message
        );
    }
    // A change request over the signed branch evaluates and lands.
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&signed), "title": "signed" }),
        )
        .await
        .unwrap();
    let id = cr["id"].as_str().unwrap().to_string();
    let ev = t
        .call(
            "ChangeService",
            "EvaluateLanding",
            json!({ "repo": repo(), "changeRequestId": id }),
        )
        .await
        .unwrap();
    assert!(
        !ev.to_string().contains("signed:"),
        "the signed stack satisfies require_signed: {ev}"
    );
    t.call(
        "ChangeService",
        "LandChange",
        json!({ "repo": repo(), "id": id }),
    )
    .await
    .expect("signed CR lands");
    assert_eq!(
        t.read("signed.txt", &t.tip("main").await.0).await.unwrap(),
        b"x\n"
    );
}

/// Commits `files` onto `main` of `full` (any repo of the dev org) and lands it.
async fn seed(t: &TestServer, full: &str, files: &[(&str, &[u8])], cid: u64) {
    let rref = json!({ "fullName": full });
    let tip = t
        .call(
            "ObjectService",
            "ResolveRef",
            json!({ "repo": rref, "name": "main" }),
        )
        .await
        .expect("source tip");
    let parent = id_text(&tip["ref"]["target"]);
    let mut delta = Vec::new();
    for (path, bytes) in files {
        let id = bee_object::blob_id(bytes).to_text();
        t.call(
            "WriteService",
            "UploadObjects",
            json!({ "messages": [{ "repo": rref, "id": id_json(&id), "offset": "0", "data": B64.encode(bytes), "last": true }] }),
        )
        .await
        .expect("upload");
        delta.push(json!({ "path": B64.encode(path.as_bytes()), "put": { "mode": "MODE_FILE", "blob": id_json(&id), "size": bytes.len().to_string() } }));
    }
    let cs = t
        .call(
            "WriteService",
            "CreateChangeset",
            json!({ "repo": rref, "parents": [id_json(&parent)], "delta": delta,
                    "changeId": change_id(cid), "author": "tester",
                    "authorTime": { "name": "tester", "time": "2026-10-18T00:00:00Z" },
                    "message": "seed" }),
        )
        .await
        .expect("changeset");
    t.call(
        "WriteService",
        "Land",
        json!({ "repo": rref, "ref": "main", "head": cs["changeset"]["id"] }),
    )
    .await
    .expect("seed lands");
}

fn missing_of(e: &bee_proto::v1::ErrorDetail) -> Vec<String> {
    match &e.payload {
        Some(bee_proto::v1::error_detail::Payload::PolicyDenial(d)) => d.missing.clone(),
        _ => Vec::new(),
    }
}

/// F49-007: an org-required workflow with `enforced` refuses lands with `workflow:<name>`
/// until it passes; evaluate-only (`enforced = false`) never blocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enforced_required_workflows_block_lands_and_evaluate_only_ones_do_not() {
    let t = start_default().await;
    t.call(
        "RepoService",
        "CreateRepo",
        json!({ "org": "dev", "name": "policies", "autoInit": true }),
    )
    .await
    .expect("source repo");
    let wf = b"name: org-audit\non: pull_request\njobs:\n  audit:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo audit\n";
    seed(
        &t,
        "dev/policies",
        &[(".github/workflows/audit.yml", wf)],
        9200,
    )
    .await;
    let req = t
        .call(
            "WorkflowService",
            "CreateRequiredWorkflow",
            json!({ "org": "dev", "name": "audit", "sourceRepo": { "fullName": "dev/policies" },
                    "sourcePath": ".github/workflows/audit.yml",
                    "scope": "WORKFLOW_REQUIREMENT_SCOPE_ALL", "enforced": true }),
        )
        .await
        .expect("required workflow");
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("a.txt", b"a\n")], 9201, "a").await["id"]);
    let e = land_ref(&t, &head)
        .await
        .expect_err("blocked until audit passes");
    assert_eq!(reason(&e), "POLICY_DENIED");
    assert_eq!(missing_of(&e), vec!["workflow:audit".to_string()], "{e:?}");
    t.call(
        "WorkflowService",
        "UpdateRequiredWorkflow",
        json!({ "org": "dev", "requiredWorkflowId": req["id"], "enforced": false }),
    )
    .await
    .expect("evaluate only");
    land_ref(&t, &head)
        .await
        .expect("evaluate-only requirements never block");
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

/// F49-008: a `code_scanning` land rule refuses a land while an open alert of the tool at
/// or above the threshold sits on the code being landed; dismissing it unblocks the land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_scanning_rule_blocks_lands_with_open_alerts_above_the_threshold() {
    let t = start_default().await;
    t.land_policy(
        "[[land]]\nref = \"main\"\n[[land.code_scanning]]\ntool = \"CodeQL\"\nalerts = \"errors\"\nsecurity_alerts = \"critical\"\n",
        9300,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let head = id_text(
        &t.commit(&tip, &[("app.js", b"eval(x)\n")], 9301, "js")
            .await["id"],
    );
    let sarif = |level: &str, rule: &str| {
        json!({"version": "2.1.0", "runs": [{
            "tool": {"driver": {"name": "CodeQL", "rules": [{"id": rule, "properties": {"security-severity": "5.0"}}]}},
            "results": [{"ruleId": rule, "level": level, "message": {"text": "bad"},
                "locations": [{"physicalLocation": {"artifactLocation": {"uri": "app.js"}, "region": {"startLine": 1}}}]}]
        }]})
        .to_string()
    };
    // A warning of medium severity is below both thresholds.
    t.call(
        "SecurityService",
        "UploadSarif",
        json!({ "repo": repo(), "changeset": id_json(&head), "ref": "refs/heads/feature",
                "sarifGzip": B64.encode(gzip(sarif("warning", "js/style").as_bytes())), "category": "style" }),
    )
    .await
    .expect("upload");
    eventually(30, "the warning alert", || async {
        let l = t
            .call(
                "SecurityService",
                "ListCodeScanningAlerts",
                json!({ "repo": repo() }),
            )
            .await
            .ok()?;
        (l["alerts"].as_array()?.len() == 1).then_some(())
    })
    .await;
    // An error-level alert blocks.
    t.call(
        "SecurityService",
        "UploadSarif",
        json!({ "repo": repo(), "changeset": id_json(&head), "ref": "refs/heads/feature",
                "sarifGzip": B64.encode(gzip(sarif("error", "js/eval").as_bytes())), "category": "security" }),
    )
    .await
    .expect("upload");
    let number = eventually(30, "the error alert", || async {
        let l = t
            .call(
                "SecurityService",
                "ListCodeScanningAlerts",
                json!({ "repo": repo() }),
            )
            .await
            .ok()?;
        l["alerts"]
            .as_array()?
            .iter()
            .find(|a| a["rule"]["id"] == "js/eval")
            .map(|a| a["number"].clone())
    })
    .await;
    let e = land_ref(&t, &head).await.expect_err("blocked by the alert");
    assert_eq!(
        missing_of(&e),
        vec!["code_scanning:CodeQL".to_string()],
        "{e:?}"
    );
    t.call(
        "SecurityService",
        "UpdateCodeScanningAlert",
        json!({ "repo": repo(), "number": number, "state": "CODE_SCANNING_ALERT_STATE_DISMISSED",
                "dismissedReason": "CODE_SCANNING_DISMISS_REASON_FALSE_POSITIVE" }),
    )
    .await
    .expect("dismiss");
    land_ref(&t, &head)
        .await
        .expect("lands once the alert is dismissed");
}

/// F49-009: `require_agent_review` refuses a land until an agent has reviewed the change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn require_agent_review_blocks_until_an_agent_reviews() {
    let t = start_default().await;
    t.land_policy(
        "[[land]]\nref = \"main\"\nrequire_agent_review = true\n",
        9400,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("f.txt", b"f\n")], 9401, "f").await["id"]);
    let cr = t
        .call(
            "ChangeService",
            "OpenChange",
            json!({ "repo": repo(), "head": id_json(&head), "title": "needs a bot" }),
        )
        .await
        .unwrap();
    let id = cr["id"].as_str().unwrap().to_string();
    let e = t
        .call(
            "ChangeService",
            "LandChange",
            json!({ "repo": repo(), "id": id }),
        )
        .await
        .expect_err("no agent review yet");
    assert!(e.message.contains("review:agent"), "{e:?}");
    // A human approval is not an agent review.
    let dev = t.srv.dev.as_ref().unwrap();
    t.call_as(
        &dev.reviewer_token,
        "ChangeService",
        "SubmitReview",
        json!({ "repo": repo(), "changeRequestId": id, "revision": 1, "verdict": "VERDICT_APPROVE" }),
    )
    .await
    .expect("human review");
    let e = t
        .call(
            "ChangeService",
            "LandChange",
            json!({ "repo": repo(), "id": id }),
        )
        .await
        .expect_err("still no agent review");
    assert!(e.message.contains("review:agent"), "{e:?}");
    // An agent acting for the dev user reviews (a comment counts).
    let (agent_id, agent_tok) = agent_token(&t).await;
    let r = t
        .call(
            "RepoService",
            "AddCollaborator",
            json!({ "repo": repo(), "principalId": agent_id, "role": "REPO_ROLE_WRITE" }),
        )
        .await
        .expect("agent collaborator");
    if let Some(inv) = r["invitation"]["id"].as_str() {
        t.call_as(
            &agent_tok,
            "RepoService",
            "AcceptRepoInvitation",
            json!({ "invitationId": inv }),
        )
        .await
        .expect("accept invitation");
    }
    t.call_as(
        &agent_tok,
        "ChangeService",
        "SubmitReview",
        json!({ "repo": repo(), "changeRequestId": id, "revision": 1,
                "verdict": "VERDICT_COMMENT", "body": "looks fine" }),
    )
    .await
    .expect("agent review");
    t.call(
        "ChangeService",
        "LandChange",
        json!({ "repo": repo(), "id": id }),
    )
    .await
    .expect("lands after the agent review");
}

/// F49-041: forge team membership becomes the policy group `@<org>/<team>` for land rules
/// and WhoAmI.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_teams_are_policy_groups() {
    let t = start_with(|c| c.forge.admission_cache_ms = 0).await;
    t.call(
        "TeamService",
        "CreateTeam",
        json!({ "org": "dev", "slug": "release-captains", "name": "Release captains" }),
    )
    .await
    .expect("team");
    let user = t.dev_user();
    // The creator joins as a maintainer; start from a team without the dev user.
    t.call(
        "TeamService",
        "RemoveTeamMember",
        json!({ "org": "dev", "team": "release-captains", "principalId": user }),
    )
    .await
    .expect("leave team");
    let who = t.call("AuthService", "WhoAmI", json!({})).await.unwrap();
    assert!(
        !who["groups"].to_string().contains("release-captains"),
        "{who}"
    );
    t.land_policy(
        "[[land]]\nref = \"main\"\nallow = [\"@dev/release-captains\"]\n",
        9500,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("r.txt", b"r\n")], 9501, "r").await["id"]);
    let e = land_ref(&t, &head).await.expect_err("not a captain yet");
    assert_eq!(missing_of(&e), vec![format!("allow:{user}")], "{e:?}");
    t.call(
        "TeamService",
        "AddTeamMember",
        json!({ "org": "dev", "team": "release-captains", "principalId": user }),
    )
    .await
    .expect("join team");
    let who = t.call("AuthService", "WhoAmI", json!({})).await.unwrap();
    assert!(
        who["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g == "dev/release-captains"),
        "{who}"
    );
    land_ref(&t, &head).await.expect("captains may land");
}

/// F416-020: a ruleset limited by `repo_properties` applies only while the repo's custom
/// property values match.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rulesets_follow_repo_custom_properties() {
    let t = start_default().await;
    t.call(
        "RepoService",
        "SetCustomPropertyDefinition",
        json!({ "org": "dev", "definition": { "name": "environment",
                "type": "CUSTOM_PROPERTY_TYPE_SINGLE_SELECT", "allowedValues": ["production", "sandbox"] } }),
    )
    .await
    .expect("definition");
    t.land_policy(
        "[[rulesets]]\nname = \"prod\"\nrefs = [\"main\"]\nrequired_checks = [\"deploy-check\"]\n[rulesets.repo_properties]\nenvironment = [\"production\"]\n",
        9600,
    )
    .await;
    let (tip, _) = t.tip("main").await;
    let head = id_text(&t.commit(&tip, &[("p.txt", b"p\n")], 9601, "p").await["id"]);
    let set = |v: &str| {
        t.call(
            "RepoService",
            "SetRepoCustomProperties",
            json!({ "repo": repo(), "properties": [{ "key": "environment", "value": v }] }),
        )
    };
    set("production").await.expect("set property");
    let e = land_ref(&t, &head)
        .await
        .expect_err("production needs the check");
    assert_eq!(
        missing_of(&e),
        vec!["check:deploy-check".to_string()],
        "{e:?}"
    );
    set("sandbox").await.expect("set property");
    land_ref(&t, &head)
        .await
        .expect("sandbox repos are out of scope");
}

/// An agent created for the dev user (AuthService.CreateAgent) and a token for it.
async fn agent_token(t: &TestServer) -> (String, String) {
    let a = t
        .call(
            "AuthService",
            "CreateAgent",
            json!({ "display": "reviewbot", "runtime": "claude-code", "purpose": "reviews" }),
        )
        .await
        .expect("agent");
    let id = a["id"].as_str().unwrap().to_string();
    let m = t
        .call("AuthService", "MintToken", json!({ "principalId": id }))
        .await
        .expect("agent token");
    (id, m["token"].as_str().unwrap().to_string())
}
