//! The community ports over the other forge domains: issue conversion through the work
//! domain (the forge's default bridge), with nothing configured by the test.

use crate::common;
use bee_forge::service::ForgeServices;
use bee_proto::v1::discussion_service_server::DiscussionService;
use bee_proto::v1::issue_service_server::IssueService;
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::*;

use common::{reason, req};

fn rref(full: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(full.into())),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issues_convert_to_discussions_and_back_through_the_work_domain() {
    let env = common::env().await;
    let s = ForgeServices::new(env.forge.clone());
    let owner = env.full("owner");
    s.create_org(req(
        &owner,
        CreateOrgRequest {
            name: "acme".into(),
            display: "Acme".into(),
        },
    ))
    .await
    .unwrap();
    let repo = s
        .create_repo(req(
            &owner,
            CreateRepoRequest {
                org: "acme".into(),
                name: "app".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    env.forge
        .orgs
        .update_repo_settings(&bee_forge::Cx::system(), &repo.id, 0, |x| {
            x.has_discussions = true;
            Ok(())
        })
        .await
        .unwrap();
    let issue = s
        .create_issue(req(
            &owner,
            CreateIssueRequest {
                repo: rref("acme/app"),
                title: "Should we?".into(),
                body: "thoughts?".into(),
                labels: vec![],
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    s.create_discussion_category(req(
        &owner,
        CreateDiscussionCategoryRequest {
            repo: rref("acme/app"),
            name: "General".into(),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let d = s
        .convert_issue_to_discussion(req(
            &owner,
            ConvertIssueToDiscussionRequest {
                repo: rref("acme/app"),
                issue_number: issue.number,
                category_id: "general".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (d.title.as_str(), d.body.as_str()),
        ("Should we?", "thoughts?")
    );
    // The work domain closed the issue with a pointer to the discussion.
    let after = s
        .get_issue_by_number(req(
            &owner,
            GetIssueByNumberRequest {
                repo: rref("acme/app"),
                number: issue.number,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(after.state, IssueState::Closed as i32);
    assert_eq!(
        reason(
            s.convert_issue_to_discussion(req(
                &owner,
                ConvertIssueToDiscussionRequest {
                    repo: rref("acme/app"),
                    issue_number: 999,
                    category_id: "general".into()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    // And back: a real issue is opened in the work domain.
    let back = s
        .convert_discussion_to_issue(req(
            &owner,
            ConvertDiscussionToIssueRequest {
                repo: rref("acme/app"),
                number: d.number,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let new_number: u64 = back.issue.unwrap().id.parse().unwrap();
    assert_ne!(new_number, issue.number);
    let created = s
        .get_issue_by_number(req(
            &owner,
            GetIssueByNumberRequest {
                repo: rref("acme/app"),
                number: new_number,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(created.body.contains("Converted from discussion") && created.title == "Should we?");
}
