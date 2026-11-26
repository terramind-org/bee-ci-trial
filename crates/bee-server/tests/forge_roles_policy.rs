//! Forge repo roles reach the policy gate (SPEC §9.4, §9.6; scenarios AS-AUDIT-01, AS-AUTH-02,
//! AS-COLLAB-01): on a repo the forge manages, every principal the policy evaluates carries
//! `@repo-role/<role>` for its own repo role (no agent inheritance), and the built-in org
//! default makes `@repo-role/admin` admins and `@repo-role/maintain` maintainers. The token
//! must still allow the action, every principal of the chain must still hold it, a repo
//! `[members]` entry replaces the org default's, and a repo the forge keeps no record of is
//! governed by policy alone.

#![allow(clippy::result_large_err)]

mod common;

use bee_policy::Action;
use bee_proto::v1::ErrorDetail;
use common::*;
use serde_json::{Value, json};

const REPO: &str = "dev/playground";

fn rref(name: &str) -> Value {
    json!({ "fullName": name })
}

/// A principal of `kind` stored in the server's table (in the dev org, owned by `owner`) with
/// `groups` on its record, and its id.
async fn stored_principal(
    t: &TestServer,
    kind: &str,
    display: &str,
    owner: &str,
    groups: &[&str],
) -> String {
    let state = &t.srv.state;
    let id = bee_server::util::new_ulid(state.now_ms())
        .unwrap()
        .to_text();
    let rec = bee_server::auth::PrincipalRecord {
        id: id.clone(),
        kind: kind.into(),
        display: display.into(),
        org: t.srv.dev.as_ref().unwrap().org.0.to_text(),
        owner: owner.into(),
        runtime: if kind == "agent" {
            "claude-code".into()
        } else {
            String::new()
        },
        purpose: String::new(),
        external_ids: Vec::new(),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        deactivated: false,
    };
    state
        .kv
        .put(
            bee_server::auth::NS_PRINCIPAL,
            id.as_bytes(),
            serde_json::to_vec(&rec).unwrap(),
        )
        .await
        .unwrap();
    id
}

/// A token for the stored principal `id` acting for `chain` (humans), limited to `repos` and
/// `actions`; an unrestricted root token when both are empty.
async fn token(
    t: &TestServer,
    id: &str,
    chain: &[&str],
    repos: &[&str],
    actions: &[Action],
) -> String {
    let state = &t.srv.state;
    let principal = state
        .principal(id)
        .await
        .unwrap()
        .expect("stored")
        .to_auth();
    let now = state.now_ms() / 1000;
    let mut scope = bee_auth::Scope::new().expires_at(now + 1800);
    if !repos.is_empty() {
        scope = scope.repos(repos.iter().copied()).unwrap();
    }
    if !actions.is_empty() {
        scope = scope.actions(actions.iter().copied());
    }
    state
        .ring
        .read()
        .unwrap()
        .mint(
            &bee_auth::MintRequest {
                principal,
                on_behalf_of: chain
                    .iter()
                    .map(|c| bee_auth::PrincipalRef::new(*c, bee_policy::PrincipalKind::Human))
                    .collect(),
                scope,
            },
            now,
        )
        .unwrap()
        .text
}

/// Adds `id` as a collaborator on `repo` with `role`, accepting the invitation as them.
async fn collaborate(t: &TestServer, repo: &str, id: &str, role: &str) {
    let rec = t.srv.state.principal(id).await.unwrap().expect("stored");
    let owner = rec.owner.clone();
    let r = t
        .call(
            "RepoService",
            "AddCollaborator",
            json!({ "repo": rref(repo), "principalId": id, "role": role }),
        )
        .await
        .unwrap_or_else(|e| panic!("add collaborator {role}: {e:?}"));
    if let Some(inv) = r["invitation"]["id"].as_str() {
        let chain: Vec<&str> = if owner.is_empty() {
            vec![]
        } else {
            vec![owner.as_str()]
        };
        let own = token(t, id, &chain, &[], &[]).await;
        t.call_as(
            &own,
            "RepoService",
            "AcceptRepoInvitation",
            json!({ "invitationId": inv }),
        )
        .await
        .expect("accept invitation");
    }
}

fn ruleset(repo: &str, name: &str) -> Value {
    json!({
        "repo": rref(repo),
        "name": name,
        "enforcement": "RULESET_ENFORCEMENT_EVALUATE",
        "conditions": { "refNameIncludes": ["~DEFAULT_BRANCH"] },
        "rules": [{ "requiredApprovals": { "count": 1 } }],
    })
}

async fn create_ruleset(
    t: &TestServer,
    tok: &str,
    repo: &str,
    name: &str,
) -> Result<Value, ErrorDetail> {
    t.call_as(tok, "PolicyService", "CreateRuleset", ruleset(repo, name))
        .await
}

/// Asserts a `POLICY_DENIED` from the policy's `[members]` naming `who` and `action`.
fn assert_role_denied(r: Result<Value, ErrorDetail>, who: &str, action: &str) {
    let e = r.expect_err("denied by policy");
    assert_eq!(reason(&e), "POLICY_DENIED", "{e:?}");
    assert!(
        e.message
            .contains(&format!("{who} holds no role granting `{action}`")),
        "{e:?}"
    );
}

async fn create_release(t: &TestServer, tok: &str, tag: &str) -> Result<Value, ErrorDetail> {
    t.call_as(
        tok,
        "ReleaseService",
        "CreateRelease",
        json!({ "repo": rref(REPO), "tagName": tag, "target": { "expr": "main" }, "name": tag }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forge_repo_roles_reach_the_policy_gate() {
    let t = start_default().await;
    // Agents are not org members; with base role none only their collaborator role counts.
    t.call(
        "OrgService",
        "UpdateOrgSettings",
        json!({ "org": "dev", "baseRepoRole": "REPO_ROLE_NONE" }),
    )
    .await
    .expect("base role none");
    // Every agent is owned by the dev user, who owns the org (an org admin).
    let owner = t.dev_user();
    let admin_bot = stored_principal(&t, "agent", "admin-bot", &owner, &[]).await;
    let write_bot = stored_principal(&t, "agent", "write-bot", &owner, &[]).await;
    let maint_bot = stored_principal(&t, "agent", "maint-bot", &owner, &[]).await;
    let plain_bot = stored_principal(&t, "agent", "plain-bot", &owner, &[]).await;
    // A stored group shaped like a repo-role group is not one.
    let forger = stored_principal(&t, "agent", "forger", &owner, &["repo-role/admin"]).await;
    collaborate(&t, REPO, &admin_bot, "REPO_ROLE_ADMIN").await;
    collaborate(&t, REPO, &write_bot, "REPO_ROLE_WRITE").await;
    collaborate(&t, REPO, &maint_bot, "REPO_ROLE_MAINTAIN").await;

    let policy_actions = [Action::Read, Action::ManagePolicy, Action::ManageSettings];

    // (a) A repo ADMIN agent manages rulesets: create, update, simulate.
    let admin = token(&t, &admin_bot, &[&owner], &[REPO], &policy_actions).await;
    let rs = create_ruleset(&t, &admin, REPO, "one-approval")
        .await
        .expect("repo admin creates a ruleset");
    let id = rs["id"].as_str().unwrap().to_string();
    let updated = t
        .call_as(
            &admin,
            "PolicyService",
            "UpdateRuleset",
            json!({ "rulesetId": id, "name": "one-approval-renamed" }),
        )
        .await
        .expect("repo admin updates the ruleset");
    assert_eq!(updated["name"], "one-approval-renamed");
    t.call_as(
        &admin,
        "PolicyService",
        "SimulateRuleset",
        json!({ "rulesetId": id, "repo": rref(REPO) }),
    )
    .await
    .expect("repo admin simulates the ruleset");
    // (b) A repo WRITE agent does not, though its owner is an org admin: no inheritance.
    let write = token(&t, &write_bot, &[&owner], &[REPO], &policy_actions).await;
    assert_role_denied(
        create_ruleset(&t, &write, REPO, "w").await,
        &write_bot,
        "manage_policy",
    );
    assert_role_denied(
        t.call_as(
            &write,
            "PolicyService",
            "SimulateRuleset",
            json!({ "rulesetId": id, "repo": rref(REPO) }),
        )
        .await,
        &write_bot,
        "manage_policy",
    );
    // An agent with no role of its own gets nothing from its org-admin owner either.
    let plain = token(&t, &plain_bot, &[&owner], &[REPO], &policy_actions).await;
    assert_role_denied(
        create_ruleset(&t, &plain, REPO, "p").await,
        &plain_bot,
        "manage_policy",
    );
    // Nor does a stored `repo-role/admin` group.
    let forged = token(&t, &forger, &[&owner], &[REPO], &policy_actions).await;
    assert_role_denied(
        create_ruleset(&t, &forged, REPO, "f").await,
        &forger,
        "manage_policy",
    );
    // A MAINTAIN agent maintains but does not administer.
    let maint_policy = token(&t, &maint_bot, &[&owner], &[REPO], &policy_actions).await;
    assert_role_denied(
        create_ruleset(&t, &maint_policy, REPO, "m").await,
        &maint_bot,
        "manage_policy",
    );

    // (d) The token still decides: a repo ADMIN agent without `manage_policy` in its token.
    let no_policy = token(
        &t,
        &admin_bot,
        &[&owner],
        &[REPO],
        &[Action::Read, Action::ManageSettings],
    )
    .await;
    let e = create_ruleset(&t, &no_policy, REPO, "t")
        .await
        .expect_err("token lacks manage_policy");
    assert_eq!(reason(&e), "POLICY_DENIED", "{e:?}");
    assert!(e.message.contains("manage_policy"), "{e:?}");
    assert!(!e.message.contains("holds no role"), "{e:?}");

    // (c) A MAINTAIN agent with `manage_refs` cuts a release (its tag at main); WRITE cannot.
    let release_actions = [Action::Read, Action::Commit, Action::ManageRefs];
    let maint = token(&t, &maint_bot, &[&owner], &[REPO], &release_actions).await;
    let rel = create_release(&t, &maint, "v1.0.0")
        .await
        .expect("maintainer creates the release");
    assert_eq!(rel["tagName"], "v1.0.0");
    t.call_as(
        &maint,
        "ReleaseService",
        "GetReleaseByTag",
        json!({ "repo": rref(REPO), "tagName": "v1.0.0" }),
    )
    .await
    .expect("the release is there");
    let write_rel = token(&t, &write_bot, &[&owner], &[REPO], &release_actions).await;
    assert_role_denied(
        create_release(&t, &write_rel, "v1.0.1").await,
        &write_bot,
        "manage_refs",
    );

    // Every principal of the chain still needs the action: a repo ADMIN agent acting for a
    // human who holds no role on the repo is refused (by repo access, before policy).
    let stranger = stored_principal(&t, "human", "stranger", "", &[]).await;
    let strangers_bot = stored_principal(&t, "agent", "strangers-bot", &stranger, &[]).await;
    collaborate(&t, REPO, &strangers_bot, "REPO_ROLE_ADMIN").await;
    let for_stranger = token(&t, &strangers_bot, &[&stranger], &[REPO], &policy_actions).await;
    assert!(
        create_ruleset(&t, &for_stranger, REPO, "s").await.is_err(),
        "a chain with a principal lacking the role is refused"
    );

    // (e) A repo `[members]` entry replaces the org default's: with `admin` naming only the
    // dev user, `@repo-role/admin` no longer administers, while the org default's
    // `maintain = ["@repo-role/maintain"]`, which the repo leaves alone, still applies.
    let (tip, _) = t.tip("main").await;
    let file = format!("version = 1\n[members]\nadmin = [\"{owner}\"]\n");
    let cs = t
        .commit(
            &tip,
            &[(".beehive/policy.toml", file.as_bytes())],
            901,
            "narrow admin",
        )
        .await;
    t.land(&id_text(&cs["id"])).await.expect("policy lands");
    assert_role_denied(
        create_ruleset(&t, &admin, REPO, "after-narrowing").await,
        &admin_bot,
        "manage_policy",
    );
    create_ruleset(&t, &t.token, REPO, "dev-user")
        .await
        .expect("the listed admin still manages policy");
    create_release(&t, &maint, "v1.1.0")
        .await
        .expect("maintainers still release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repos_the_forge_does_not_manage_are_governed_by_policy_alone() {
    let t = start_default().await;
    let state = &t.srv.state;
    let org = state
        .meta
        .org_by_name("dev")
        .await
        .unwrap()
        .expect("dev org");
    let row = bee_store::RepoRow {
        id: bee_store::RepoId(bee_server::util::new_ulid(state.now_ms()).unwrap()),
        org: org.id,
        name: "vcs-only".into(),
        created_at_ms: state.now_ms(),
        draft_retention_days: bee_store::DEFAULT_DRAFT_RETENTION_DAYS,
    };
    bee_server::server::create_repo(state, &org, &row, true)
        .await
        .expect("VCS-only repo");
    let owner = t.dev_user();
    let admin_bot = stored_principal(&t, "agent", "admin-bot", &owner, &[]).await;
    // An ADMIN collaborator elsewhere: the role is per repo, and this repo has no record.
    collaborate(&t, REPO, &admin_bot, "REPO_ROLE_ADMIN").await;
    let actions = [Action::Read, Action::ManagePolicy, Action::ManageSettings];
    let admin = token(&t, &admin_bot, &[&owner], &["dev/vcs-only"], &actions).await;
    assert_role_denied(
        create_ruleset(&t, &admin, "dev/vcs-only", "a").await,
        &admin_bot,
        "manage_policy",
    );
    // As before: the org admins the built-in default lists by id administer.
    create_ruleset(&t, &t.token, "dev/vcs-only", "dev-user")
        .await
        .expect("an org admin manages policy");
}
