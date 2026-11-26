//! Repo-scoped agent tokens on forge RPCs, end to end on a running server.
//!
//! An agent token lists its repos either by full name (`dev/playground`, as
//! `docs/prompts/mint-agent-token.md` teaches and as core's gate checks) or by repo id (as the
//! forge's own minters write them, and as `TokenScope.repos` allows). Either form must open a
//! change request, set its brief and post, file an issue and list changes on the repo it names,
//! and each of those calls must be refused with `POLICY_DENIED` on any other repo, which the
//! agent can otherwise write to.

use crate::scale_support;

use bee_conformance::load::client::{Client, RpcError, change_id, pid};
use bee_proto::v1;
use bee_proto::v1::brief_service_client::BriefServiceClient;
use bee_proto::v1::change_service_client::ChangeServiceClient;
use bee_proto::v1::issue_service_client::IssueServiceClient;
use bee_proto::v1::post_service_client::PostServiceClient;
use bee_proto::v1::repo_service_client::RepoServiceClient;
use scale_support::*;

const SCOPED: &str = "dev/playground";
const OTHER: &str = "dev/other";

fn reason(e: tonic::Status) -> String {
    RpcError::from(e).reason()
}

/// The id of the repo `full_name`.
async fn repo_id(c: &Client, full_name: &str) -> String {
    RepoServiceClient::new(c.channel().clone())
        .get_repo(c.req(v1::GetRepoRequest {
            repo: Some(v1::RepoRef {
                repo: Some(v1::repo_ref::Repo::FullName(full_name.into())),
            }),
        }))
        .await
        .expect("get repo")
        .into_inner()
        .id
}

/// A token for `principal_id` limited to `repos` and to writes under `/docs`.
async fn mint(root: &Client, principal_id: &str, repos: &[String]) -> String {
    v1::auth_service_client::AuthServiceClient::new(root.channel().clone())
        .mint_token(root.req(v1::MintTokenRequest {
            principal_id: principal_id.into(),
            scope: Some(v1::TokenScope {
                repos: repos.to_vec(),
                write_paths: vec![b"/docs".to_vec()],
                budget: Some(v1::Budget {
                    max_changesets: 100,
                    max_landed: 0,
                    max_upload_bytes: 0,
                }),
                ..Default::default()
            }),
        }))
        .await
        .expect("mint")
        .into_inner()
        .token
}

/// A changeset on top of `main` of `c`'s repo.
async fn commit(c: &Client, n: u64) -> bee_core::ObjectId {
    let (tip, _) = main_tip(c).await;
    c.commit(
        &tip,
        &[(format!("docs/{n}.md"), format!("note {n}\n").into_bytes())],
        change_id(n),
        &format!("note {n}"),
    )
    .await
    .unwrap_or_else(|e| panic!("commit {n}: {e:?}"))
}

/// The five forge calls on `c`'s repo, each returning its error's reason.
struct Calls<'a> {
    c: &'a Client,
}

impl Calls<'_> {
    async fn open_change(&self, head: &bee_core::ObjectId) -> Result<String, String> {
        ChangeServiceClient::new(self.c.channel().clone())
            .open_change(self.c.req(v1::OpenChangeRequest {
                repo: self.c.repo(),
                target_ref: "main".into(),
                head: Some(pid(head)),
                title: "Scoped change".into(),
                ..Default::default()
            }))
            .await
            .map(|r| r.into_inner().id)
            .map_err(reason)
    }

    async fn set_brief(&self, change_id: &str) -> Result<(), String> {
        BriefServiceClient::new(self.c.channel().clone())
            .set_change_brief(self.c.req(v1::SetChangeBriefRequest {
                repo: self.c.repo(),
                change_id: change_id.into(),
                brief: Some(v1::ChangeBrief {
                    outcome: "Notes are easier to find.".into(),
                    ..Default::default()
                }),
                expected_version: 0,
            }))
            .await
            .map(|_| ())
            .map_err(reason)
    }

    async fn set_post(&self, change_id: &str) -> Result<(), String> {
        PostServiceClient::new(self.c.channel().clone())
            .set_change_post(self.c.req(v1::SetChangePostRequest {
                repo: self.c.repo(),
                change_id: change_id.into(),
                text: "Notes now live under docs/.".into(),
                ..Default::default()
            }))
            .await
            .map(|_| ())
            .map_err(reason)
    }

    async fn create_issue(&self) -> Result<(), String> {
        IssueServiceClient::new(self.c.channel().clone())
            .create_issue(self.c.req(v1::CreateIssueRequest {
                repo: self.c.repo(),
                title: "Index the notes".into(),
                ..Default::default()
            }))
            .await
            .map(|_| ())
            .map_err(reason)
    }

    async fn list_changes(&self) -> Result<usize, String> {
        self.c
            .list_changes()
            .await
            .map(|v| v.len())
            .map_err(|e| e.reason())
    }
}

/// Runs the five calls with `token` on [`SCOPED`] (all succeed) and on [`OTHER`] (all are
/// refused). `scope` builds the token's repo list from the two repos' full names and ids.
async fn scoped_token_reaches_only_its_repo(scope: impl Fn(&str, &str) -> String) {
    let (s, _dir) = start_fresh(|_| {}, None).await;
    RepoServiceClient::new(s.client.channel().clone())
        .create_repo(s.client.req(v1::CreateRepoRequest {
            org: "dev".into(),
            name: "other".into(),
            visibility: v1::Visibility::Private as i32,
            auto_init: true,
            ..Default::default()
        }))
        .await
        .expect("create other");
    let root = &s.client;
    let root_other = Client::new(root.channel().clone(), root.token(), OTHER);

    // The agent may write to both repos: only its token keeps it to one.
    let agent = root.create_agent("scoped").await.expect("agent");
    root.add_member("dev", &agent).await.expect("member");
    root.add_writer(&agent).await.expect("writer on scoped");
    root_other
        .add_writer(&agent)
        .await
        .expect("writer on other");

    let scoped_id = repo_id(root, SCOPED).await;
    let other_id = repo_id(root, OTHER).await;
    assert_ne!(scoped_id, other_id);
    let token = mint(root, &agent, &[scope(SCOPED, &scoped_id)]).await;
    // Changesets come from an unrestricted agent token: core checks names only, and this test
    // is about the forge calls.
    let free = mint(root, &agent, &[]).await;

    let on_scoped = Client::new(root.channel().clone(), &token, SCOPED);
    let head = commit(&on_scoped.with_token(&free), 1).await;
    let ok = Calls { c: &on_scoped };
    let cr = ok.open_change(&head).await.expect("open change on scoped");
    ok.set_brief(&cr).await.expect("set brief on scoped");
    ok.set_post(&cr).await.expect("set post on scoped");
    ok.create_issue().await.expect("create issue on scoped");
    assert_eq!(ok.list_changes().await.expect("list on scoped"), 1);

    // On the other repo the agent's unrestricted token can do all of it; the scoped one none.
    let free_other = Client::new(root.channel().clone(), &free, OTHER);
    let other_head = commit(&free_other, 2).await;
    let other_cr = Calls { c: &free_other }
        .open_change(&other_head)
        .await
        .expect("open change on other, unrestricted");
    let other_head2 = commit(&free_other, 3).await;
    let no = Calls {
        c: &free_other.with_token(&token),
    };
    let denied = "POLICY_DENIED".to_string();
    assert_eq!(no.open_change(&other_head2).await, Err(denied.clone()));
    assert_eq!(no.set_brief(&other_cr).await, Err(denied.clone()));
    assert_eq!(no.set_post(&other_cr).await, Err(denied.clone()));
    assert_eq!(no.create_issue().await, Err(denied.clone()));
    assert_eq!(no.list_changes().await, Err(denied));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_scoped_by_full_name_reaches_only_that_repo_on_the_forge() {
    scoped_token_reaches_only_its_repo(|name, _id| name.to_string()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_token_scoped_by_repo_id_reaches_only_that_repo_on_the_forge() {
    scoped_token_reaches_only_its_repo(|_name, id| id.to_string()).await;
}
