//! The enterprise domains wired into the work and changes modules: moderation as the write
//! guard of issues and comments, and teams as the changes module's team directory.

use crate::common;
use crate::enterprise_support as support;

use std::sync::Arc;

use bee_forge::changes::TeamDirectory as ChangesTeams;
use bee_forge::enterprise::moderation::blocks::{BlockTarget, NewBlock};
use bee_forge::enterprise::moderation::limits::{LimitTarget, NewLimit};
use bee_forge::enterprise::moderation::{InMemoryStanding, Moderation, ModerationConfig};
use bee_forge::enterprise::teams::{Invitee, NewTeam, ReviewPatch};
use bee_forge::service::ForgeServices;
use bee_proto::v1::issue_service_server::IssueService;
use bee_proto::v1::{
    CreateIssueCommentRequest, CreateIssueRequest, InteractionLimitLevel, RepoRef, Role,
    TeamMemberRole, repo_ref,
};
use support::{World, add_member, cx, make_org, make_repo};

fn repo_ref_of(id: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::Id(id.into())),
    })
}

async fn moderation_guards_issue_and_comment_writes(w: World) {
    let org = make_org(&w.env, "acme").await;
    for (id, role) in [("writer", Role::Member), ("reader", Role::Member)] {
        add_member(&w.env, &org, id, role).await;
    }
    let repo = make_repo(&w.env, &org, "app").await;
    let owner = cx("owner");
    let m = Moderation::new(
        w.env.forge.clone(),
        ModerationConfig::new(Arc::new(InMemoryStanding::new())),
    );
    let svc = ForgeServices::new(w.env.forge.clone());
    let create = |who: &str| {
        common::req(
            &w.env.full(who),
            CreateIssueRequest {
                repo: repo_ref_of(&repo.id),
                title: "bug".into(),
                ..Default::default()
            },
        )
    };
    // Without a guard installed everything passes.
    let first = svc
        .create_issue(create("reader"))
        .await
        .unwrap()
        .into_inner();

    w.env.forge.set_write_guard(m.clone());
    let reader = w.env.forge.hooks.directory.get("reader").await.unwrap();

    // An interaction limit stops newcomers but not collaborators.
    m.set_limit(
        &owner,
        &LimitTarget::Repo(repo.id.clone()),
        &org.id,
        NewLimit {
            level: InteractionLimitLevel::CollaboratorsOnly,
            duration_secs: 3600,
            applies_to_agents: true,
        },
    )
    .await
    .unwrap();
    // The reader is an org member with base read: not a collaborator in the limit's sense.
    let e = common::reason(svc.create_issue(create("reader")).await);
    assert_eq!(e, "INTERACTION_LIMITED");
    assert!(
        svc.create_issue(create("owner")).await.is_ok(),
        "owners are collaborators"
    );
    let comment = |who: &str| {
        common::req(
            &w.env.full(who),
            CreateIssueCommentRequest {
                repo: repo_ref_of(&repo.id),
                number: first.number,
                body: "hi".into(),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        common::reason(svc.create_issue_comment(comment("reader")).await),
        "INTERACTION_LIMITED"
    );
    assert!(svc.create_issue_comment(comment("owner")).await.is_ok());
    m.remove_limit(&owner, &LimitTarget::Repo(repo.id.clone()), &org.id)
        .await
        .unwrap();
    assert!(svc.create_issue_comment(comment("reader")).await.is_ok());

    // Blocked users cannot write.
    m.block(
        &owner,
        &BlockTarget::Org(org.id.clone()),
        NewBlock {
            blocked: reader,
            reason: "spam".into(),
            note: String::new(),
            duration_secs: 0,
            source: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        common::reason(svc.create_issue(create("reader")).await),
        "BLOCKED"
    );
    assert_eq!(
        common::reason(svc.create_issue_comment(comment("reader")).await),
        "BLOCKED"
    );
    assert!(svc.create_issue(create("writer")).await.is_ok());
}
on_both_backends!(moderation_guards_issue_and_comment_writes);

async fn teams_resolve_review_requests_for_the_changes_module(w: World) {
    let org = make_org(&w.env, "acme").await;
    for id in ["writer", "reader"] {
        add_member(&w.env, &org, id, Role::Member).await;
    }
    let owner = cx("owner");
    let t = &w.ent.teams;
    t.create(
        &owner,
        &org,
        NewTeam {
            slug: "rev".into(),
            name: "rev".into(),
            maintainers: vec!["owner".into()],
            ..NewTeam::default()
        },
    )
    .await
    .unwrap();
    t.add_member(
        &owner,
        &org,
        "rev",
        Invitee::Principal("writer".into()),
        TeamMemberRole::Member,
        None,
    )
    .await
    .unwrap();
    t.create(
        &owner,
        &org,
        NewTeam {
            slug: "kids".into(),
            name: "kids".into(),
            parent: "rev".into(),
            maintainers: vec!["owner".into()],
            ..NewTeam::default()
        },
    )
    .await
    .unwrap();
    t.add_member(
        &owner,
        &org,
        "kids",
        Invitee::Principal("reader".into()),
        TeamMemberRole::Member,
        None,
    )
    .await
    .unwrap();

    let dir: Arc<dyn ChangesTeams> = t.clone();
    let mut ids: Vec<String> = dir
        .members(&org.id, "rev")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec!["owner", "reader", "writer"],
        "members including child teams"
    );
    assert!(
        dir.assignment(&org.id, "rev").await.unwrap().is_none(),
        "nothing configured yet"
    );
    t.update_review_assignment(
        &owner,
        &org,
        "rev",
        ReviewPatch {
            enabled: Some(true),
            member_count: Some(2),
            ..ReviewPatch::default()
        },
    )
    .await
    .unwrap();
    let a = dir.assignment(&org.id, "rev").await.unwrap().unwrap();
    assert!(a.enabled && a.member_count == 2);
    assert!(dir.members(&org.id, "nope").await.is_err());
}
on_both_backends!(teams_resolve_review_requests_for_the_changes_module);
