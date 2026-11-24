//! Teams, custom roles and team repo access: lifecycle and nesting, membership and
//! invitations, access through `AccessSource` (with inheritance), custom role evaluation
//! through `bee-policy`, review assignment, IdP group links, and the `TeamService`
//! permission matrix. Store flows run on memory and SQLite.

use crate::common;
use crate::enterprise_support as support;

use std::collections::BTreeMap;

use bee_forge::enterprise::teams::{AddOutcome, Invitee, NewTeam, ReviewPatch, TeamPatch, Viewer};
use bee_policy::Action;
use bee_proto::v1::team_service_server::TeamService;
use bee_proto::v1::{
    AddTeamMemberRequest, CreateTeamRequest, ErrorReason, GetTeamRequest,
    GrantTeamRepoAccessRequest, LinkTeamExternalGroupRequest, ListTeamsRequest, PolicyRoleKind,
    RepoRef, RepoRole, Role, TeamMemberRole, TeamPrivacy, TeamReviewAlgorithm, repo_ref,
};
use support::{World, add_member, cx, make_org, make_repo};

fn viewer(ids: &[&str], owner: bool) -> Viewer {
    Viewer {
        ids: ids.iter().map(|s| s.to_string()).collect(),
        org_owner: owner,
    }
}

fn new_team(slug: &str, parent: &str) -> NewTeam {
    NewTeam {
        slug: slug.into(),
        name: slug.into(),
        parent: parent.into(),
        maintainers: vec!["owner".into()],
        ..NewTeam::default()
    }
}

async fn members(w: &World, org: &bee_proto::v1::Org) {
    for (id, role) in [
        ("admin", Role::Admin),
        ("writer", Role::Member),
        ("reader", Role::Member),
    ] {
        add_member(&w.env, org, id, role).await;
    }
}

async fn teams_lifecycle(w: World) {
    let org = make_org(&w.env, "acme").await;
    members(&w, &org).await;
    let t = &w.ent.teams;
    let owner = cx("owner");

    let core = t
        .create(
            &owner,
            &org,
            NewTeam {
                name: "Core Team".into(),
                maintainers: vec!["owner".into()],
                ..NewTeam::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(core.slug, "core-team");
    assert_eq!(core.policy_group, "@acme/core-team");
    assert_eq!(core.member_count, 1);
    assert_eq!(core.org, "acme");
    assert_eq!(
        t.create(&owner, &org, new_team("core-team", ""))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    assert_eq!(
        t.create(&owner, &org, new_team("Bad Slug", ""))
            .await
            .unwrap_err()
            .subcode,
        "invalid_name"
    );
    let infra = t
        .create(&owner, &org, new_team("infra", "core-team"))
        .await
        .unwrap();
    let deep = t
        .create(&owner, &org, new_team("deep", "infra"))
        .await
        .unwrap();
    assert_eq!(deep.parent_team_id, infra.id);

    // No cycles: a team cannot move below its own descendant.
    let e = t
        .update(
            &owner,
            &org,
            "core-team",
            TeamPatch {
                parent: Some("deep".into()),
                ..TeamPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "team_cycle");
    let e = t
        .update(
            &owner,
            &org,
            "infra",
            TeamPatch {
                parent: Some("infra".into()),
                ..TeamPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.subcode, "team_cycle");

    // Secret teams cannot be nested.
    let secret = NewTeam {
        privacy: TeamPrivacy::Secret,
        ..new_team("hush", "")
    };
    let hush = t.create(&owner, &org, secret).await.unwrap();
    assert_eq!(
        t.create(
            &owner,
            &org,
            NewTeam {
                privacy: TeamPrivacy::Secret,
                ..new_team("hush2", "core-team")
            }
        )
        .await
        .unwrap_err()
        .subcode,
        "secret_team_nesting"
    );
    assert_eq!(
        t.create(&owner, &org, new_team("under-hush", "hush"))
            .await
            .unwrap_err()
            .subcode,
        "secret_team_nesting"
    );
    assert_eq!(
        t.update(
            &owner,
            &org,
            "core-team",
            TeamPatch {
                privacy: Some(TeamPrivacy::Secret),
                ..TeamPatch::default()
            }
        )
        .await
        .unwrap_err()
        .subcode,
        "secret_team_nesting"
    );

    // Secret teams are invisible to non-members, visible to members and owners.
    assert_eq!(
        t.get(&owner, &org, "hush", &viewer(&["reader"], false))
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    assert!(
        t.get(&owner, &org, "hush", &viewer(&["owner"], true))
            .await
            .is_ok()
    );
    assert!(
        t.get(&owner, &org, &hush.id, &viewer(&["owner"], false))
            .await
            .is_ok()
    );
    let (listed, _) = t
        .list(&owner, &org, "", false, &viewer(&["reader"], false), "", 50)
        .await
        .unwrap();
    assert!(listed.iter().all(|x| x.slug != "hush"));
    let (all, _) = t
        .list(&owner, &org, "", false, &viewer(&["owner"], true), "", 50)
        .await
        .unwrap();
    assert_eq!(all.len(), 4);
    let (top, _) = t
        .list(&owner, &org, "", true, &viewer(&["owner"], true), "", 50)
        .await
        .unwrap();
    assert_eq!(
        top.iter()
            .map(|x| x.slug.as_str())
            .collect::<Vec<_>>()
            .len(),
        2
    );
    let (found, _) = t
        .list(
            &owner,
            &org,
            "dee",
            false,
            &viewer(&["owner"], true),
            "",
            50,
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 1);

    // Children, direct and recursive; counts.
    let v = viewer(&["owner"], true);
    let (direct, _) = t
        .children(&owner, &org, "core-team", false, &v, "", 50)
        .await
        .unwrap();
    assert_eq!(direct.len(), 1);
    let (rec, _) = t
        .children(&owner, &org, "core-team", true, &v, "", 50)
        .await
        .unwrap();
    assert_eq!(rec.len(), 2);
    assert_eq!(
        t.get(&owner, &org, "core-team", &v)
            .await
            .unwrap()
            .child_team_count,
        1
    );

    // Rename and re-parent; deleting a team moves its children up.
    let renamed = t
        .update(
            &owner,
            &org,
            "deep",
            TeamPatch {
                slug: Some("deeper".into()),
                name: Some("Deeper".into()),
                parent: Some(String::new()),
                ..TeamPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.slug, "deeper");
    assert!(renamed.parent_team_id.is_empty());
    t.update(
        &owner,
        &org,
        "deeper",
        TeamPatch {
            parent: Some("infra".into()),
            ..TeamPatch::default()
        },
    )
    .await
    .unwrap();
    t.delete(&owner, &org, "infra").await.unwrap();
    let moved = t.get(&owner, &org, "deeper", &v).await.unwrap();
    assert_eq!(moved.parent_team_id, core.id);
    assert_eq!(
        t.get(&owner, &org, "infra", &v).await.unwrap_err().reason,
        ErrorReason::NotFound
    );

    let actions = support::audit_actions(&w, &format!("org:{}", org.id)).await;
    for a in ["team.create", "team.update", "team.delete"] {
        assert!(
            actions.iter().any(|x| x == a),
            "missing audit action {a}: {actions:?}"
        );
    }
}
on_both_backends!(teams_lifecycle);

async fn membership_and_invitations(w: World) {
    let org = make_org(&w.env, "acme").await;
    members(&w, &org).await;
    let t = &w.ent.teams;
    let owner = cx("owner");
    t.create(&owner, &org, new_team("core", "")).await.unwrap();
    t.create(&owner, &org, new_team("kids", "core"))
        .await
        .unwrap();
    let v = viewer(&["owner"], true);

    // An org member is added at once; re-adding changes the role.
    let AddOutcome::Member(m) = t
        .add_member(
            &owner,
            &org,
            "core",
            Invitee::Principal("writer".into()),
            TeamMemberRole::Member,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("expected a membership");
    };
    assert_eq!(m.role, TeamMemberRole::Member as i32);
    t.add_member(
        &owner,
        &org,
        "core",
        Invitee::Principal("writer".into()),
        TeamMemberRole::Maintainer,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        t.membership(&owner, &org, "core", "writer")
            .await
            .unwrap()
            .role,
        TeamMemberRole::Maintainer as i32
    );
    assert!(
        t.is_maintainer(&owner, &org.id, "kids", "writer")
            .await
            .unwrap(),
        "maintainers of a parent maintain its children"
    );
    assert!(
        !t.is_maintainer(&owner, &org.id, "core", "reader")
            .await
            .unwrap()
    );

    // Someone who is not an org member (or an email) gets a pending invitation.
    let AddOutcome::Invitation(inv) = t
        .add_member(
            &owner,
            &org,
            "core",
            Invitee::Principal("outsider".into()),
            TeamMemberRole::Member,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("expected an invitation");
    };
    assert_eq!(inv.invitee_principal_id, "outsider");
    assert!(matches!(
        t.add_member(
            &owner,
            &org,
            "core",
            Invitee::Email("New@Example.com".into()),
            TeamMemberRole::Member,
            None
        )
        .await
        .unwrap(),
        AddOutcome::Invitation(_)
    ));
    assert_eq!(
        t.add_member(
            &owner,
            &org,
            "core",
            Invitee::Email("new@example.com".into()),
            TeamMemberRole::Member,
            None
        )
        .await
        .unwrap_err()
        .reason,
        ErrorReason::AlreadyExists
    );
    let (pending, _) = t.invitations(&owner, &org, "core", "", 50).await.unwrap();
    assert_eq!(pending.len(), 2);
    let email_inv = pending
        .iter()
        .find(|i| !i.invitee_email.is_empty())
        .unwrap()
        .id
        .clone();
    t.cancel_invitation(&owner, &org, "core", &email_inv)
        .await
        .unwrap();
    assert_eq!(
        t.invitations(&owner, &org, "core", "", 50)
            .await
            .unwrap()
            .0
            .len(),
        1
    );

    // Joining the org turns the invitation into a membership (event listener).
    add_member(&w.env, &org, "outsider", Role::Member).await;
    w.env.forge.flush().await.unwrap();
    assert!(t.membership(&owner, &org, "core", "outsider").await.is_ok());
    assert!(
        t.invitations(&owner, &org, "core", "", 50)
            .await
            .unwrap()
            .0
            .is_empty()
    );

    // Child team members are inherited members of the parent.
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
    let (direct, _) = t
        .members(
            &owner,
            &org,
            "core",
            TeamMemberRole::Unspecified,
            false,
            &v,
            "",
            50,
        )
        .await
        .unwrap();
    assert!(
        direct
            .iter()
            .all(|m| m.principal.as_ref().unwrap().id != "reader")
    );
    let (all, _) = t
        .members(
            &owner,
            &org,
            "core",
            TeamMemberRole::Unspecified,
            true,
            &v,
            "",
            50,
        )
        .await
        .unwrap();
    let r = all
        .iter()
        .find(|m| m.principal.as_ref().unwrap().id == "reader")
        .unwrap();
    assert!(r.inherited);
    assert_eq!(
        t.membership(&owner, &org, "core", "reader")
            .await
            .unwrap()
            .via_team_id,
        r.via_team_id
    );
    assert!(
        w.ent
            .teams
            .is_member(&owner, &r.via_team_id, "reader")
            .await
            .unwrap()
    );
    let (mine, _) = t
        .principal_teams(&owner, &org, "reader", true, &v, "", 50)
        .await
        .unwrap();
    assert_eq!(mine.len(), 2, "the child team and its parent");
    let (direct_only, _) = t
        .principal_teams(&owner, &org, "reader", false, &v, "", 50)
        .await
        .unwrap();
    assert_eq!(direct_only.len(), 1);

    // Leaving the org leaves the teams.
    let writer = w.env.forge.hooks.directory.get("writer").await.unwrap();
    w.env
        .forge
        .orgs
        .set_member(&owner, &org.id, writer, Role::Unspecified)
        .await
        .unwrap();
    w.env.forge.flush().await.unwrap();
    assert_eq!(
        t.membership(&owner, &org, "core", "writer")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    t.remove_member(&owner, &org, "kids", "reader", false)
        .await
        .unwrap();
    assert_eq!(
        t.remove_member(&owner, &org, "kids", "reader", false)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
}
on_both_backends!(membership_and_invitations);

async fn pending_invitation_is_not_resolved_for_non_org_member(w: World) {
    let org = make_org(&w.env, "pending-invitation-org-member-check").await;
    let teams = &w.ent.teams;
    let owner = cx("owner");
    teams
        .create(&owner, &org, new_team("core", ""))
        .await
        .unwrap();

    let AddOutcome::Invitation(invitation) = teams
        .add_member(
            &owner,
            &org,
            "core",
            Invitee::Principal("outsider".into()),
            TeamMemberRole::Member,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("non-org principal should have a pending invitation");
    };

    assert!(
        teams
            .resolve_pending(&owner, &org, "outsider", &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        teams
            .membership(&owner, &org, "core", "outsider")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    let (pending, _) = teams
        .invitations(&owner, &org, "core", "", 50)
        .await
        .unwrap();
    assert!(
        pending
            .iter()
            .any(|candidate| candidate.id == invitation.id)
    );
}
on_both_backends!(pending_invitation_is_not_resolved_for_non_org_member);

async fn repo_access_inherits_and_feeds_effective_role(w: World) {
    let org = make_org(&w.env, "acme").await;
    members(&w, &org).await;
    // Take the org base role away so team grants are what gives access.
    w.env
        .forge
        .orgs
        .update_settings(&cx("owner"), &org.id, 0, |s| {
            s.base_repo_role = RepoRole::None as i32
        })
        .await
        .unwrap();
    let r1 = make_repo(&w.env, &org, "one").await;
    let r2 = make_repo(&w.env, &org, "two").await;
    let t = &w.ent.teams;
    let owner = cx("owner");
    t.create(&owner, &org, new_team("platform", ""))
        .await
        .unwrap();
    t.create(&owner, &org, new_team("sre", "platform"))
        .await
        .unwrap();
    t.add_member(
        &owner,
        &org,
        "sre",
        Invitee::Principal("writer".into()),
        TeamMemberRole::Member,
        None,
    )
    .await
    .unwrap();

    let before = w
        .env
        .forge
        .orgs
        .effective_role(&owner, &r1, "writer")
        .await
        .unwrap();
    assert_eq!(before.role, RepoRole::None as i32);

    // The parent team's grant reaches members of its child team.
    t.grant_repo(&owner, &org, "platform", &r1, "write")
        .await
        .unwrap();
    let perm = w
        .env
        .forge
        .orgs
        .effective_role(&owner, &r1, "writer")
        .await
        .unwrap();
    assert_eq!(perm.role, RepoRole::Write as i32);
    assert!(
        perm.sources.iter().any(|s| s == "team:platform"),
        "{:?}",
        perm.sources
    );
    assert_eq!(
        w.env
            .forge
            .orgs
            .effective_role(&owner, &r2, "writer")
            .await
            .unwrap()
            .role,
        RepoRole::None as i32
    );
    assert_eq!(
        w.env
            .forge
            .orgs
            .effective_role(&owner, &r1, "reader")
            .await
            .unwrap()
            .role,
        RepoRole::None as i32
    );

    // A child's own grant wins when higher; the highest role is reported.
    t.grant_repo(&owner, &org, "sre", &r1, "admin")
        .await
        .unwrap();
    assert_eq!(
        w.env
            .forge
            .orgs
            .effective_role(&owner, &r1, "writer")
            .await
            .unwrap()
            .role,
        RepoRole::Admin as i32
    );

    // Listings, inherited and direct.
    let (own, _) = t
        .team_repos(&owner, &org, "sre", false, "", 50)
        .await
        .unwrap();
    assert_eq!(own.len(), 1);
    t.revoke_repo(&owner, &org, "sre", &r1).await.unwrap();
    let (inh, _) = t
        .team_repos(&owner, &org, "sre", true, "", 50)
        .await
        .unwrap();
    assert_eq!(inh.len(), 1);
    assert!(inh[0].inherited);
    let (rt, _) = t.repo_teams(&owner, &r1.id, "", 50).await.unwrap();
    assert_eq!(rt.len(), 1);
    assert_eq!(
        t.get(&owner, &org, "platform", &viewer(&["owner"], true))
            .await
            .unwrap()
            .repo_count,
        1
    );

    // Unknown roles, foreign repos and double revocation are refused.
    assert_eq!(
        t.grant_repo(&owner, &org, "platform", &r2, "godmode")
            .await
            .unwrap_err()
            .subcode,
        "unknown_role"
    );
    let other = make_org(&w.env, "other").await;
    let foreign = make_repo(&w.env, &other, "x").await;
    assert_eq!(
        t.grant_repo(&owner, &org, "platform", &foreign, "read")
            .await
            .unwrap_err()
            .subcode,
        "repo_not_in_org"
    );
    assert_eq!(
        t.revoke_repo(&owner, &org, "sre", &r1)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );

    // Removing the grant removes the access.
    t.revoke_repo(&owner, &org, "platform", &r1).await.unwrap();
    assert_eq!(
        w.env
            .forge
            .orgs
            .effective_role(&owner, &r1, "writer")
            .await
            .unwrap()
            .role,
        RepoRole::None as i32
    );
}
on_both_backends!(repo_access_inherits_and_feeds_effective_role);

async fn custom_roles_are_evaluated_through_policy(w: World) {
    let org = make_org(&w.env, "acme").await;
    members(&w, &org).await;
    w.env
        .forge
        .orgs
        .update_settings(&cx("owner"), &org.id, 0, |s| {
            s.base_repo_role = RepoRole::None as i32
        })
        .await
        .unwrap();
    let repo = make_repo(&w.env, &org, "app").await;
    let roles = &w.ent.roles;
    let t = &w.ent.teams;
    let owner = cx("owner");

    let rm = roles
        .create(
            &owner,
            &org,
            PolicyRoleKind::Repo,
            "Release Manager",
            "ships releases",
            "write",
            vec!["manage_refs".into(), "lease".into()],
        )
        .await
        .unwrap();
    assert_eq!(rm.base_role, "write");
    // Validation.
    assert_eq!(
        roles
            .create(
                &owner,
                &org,
                PolicyRoleKind::Repo,
                "release manager",
                "",
                "write",
                vec![]
            )
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    assert_eq!(
        roles
            .create(
                &owner,
                &org,
                PolicyRoleKind::Repo,
                "admin",
                "",
                "write",
                vec![]
            )
            .await
            .unwrap_err()
            .subcode,
        "reserved_name"
    );
    assert_eq!(
        roles
            .create(&owner, &org, PolicyRoleKind::Repo, "x", "", "admin", vec![])
            .await
            .unwrap_err()
            .subcode,
        "invalid_base_role"
    );
    assert_eq!(
        roles
            .create(
                &owner,
                &org,
                PolicyRoleKind::Repo,
                "y",
                "",
                "read",
                vec!["purge".into()]
            )
            .await
            .unwrap_err()
            .subcode,
        "permission_not_allowed"
    );
    assert_eq!(
        roles
            .create(
                &owner,
                &org,
                PolicyRoleKind::Repo,
                "z",
                "",
                "read",
                vec!["fly".into()]
            )
            .await
            .unwrap_err()
            .subcode,
        "unknown_permission"
    );

    t.create(&owner, &org, new_team("releasers", ""))
        .await
        .unwrap();
    t.add_member(
        &owner,
        &org,
        "releasers",
        Invitee::Principal("writer".into()),
        TeamMemberRole::Member,
        None,
    )
    .await
    .unwrap();
    t.grant_repo(&owner, &org, "releasers", &repo, &rm.id)
        .await
        .unwrap();

    // The member holds the base role's actions plus the custom permissions, and nothing else.
    let access = roles.repo_actions(&owner, &repo, "writer").await.unwrap();
    for a in [
        Action::Read,
        Action::Commit,
        Action::Land,
        Action::ManageRefs,
        Action::Lease,
    ] {
        assert!(
            access.actions.contains(&a),
            "{a} missing: {:?}",
            access.actions
        );
    }
    for a in [
        Action::ForceUpdate,
        Action::ManageSettings,
        Action::Redact,
        Action::ManagePolicy,
    ] {
        assert!(!access.actions.contains(&a), "{a} must not be granted");
    }
    assert!(
        access
            .sources
            .iter()
            .any(|s| s.starts_with("custom:Release Manager")),
        "{:?}",
        access.sources
    );
    // The built-in effective role reports the base role, so listings still work.
    assert_eq!(
        w.env
            .forge
            .orgs
            .effective_role(&owner, &repo, "writer")
            .await
            .unwrap()
            .role,
        RepoRole::Write as i32
    );
    // Someone outside the team holds nothing.
    assert!(
        roles
            .repo_actions(&owner, &repo, "reader")
            .await
            .unwrap()
            .actions
            .is_empty()
    );
    roles
        .check_chain(&owner, &repo, &["writer".to_string()], Action::ManageRefs)
        .await
        .unwrap();
    assert_eq!(
        roles
            .check_chain(
                &owner,
                &repo,
                &["writer".to_string(), "reader".to_string()],
                Action::ManageRefs
            )
            .await
            .unwrap_err()
            .reason,
        ErrorReason::PermissionDenied
    );
    assert_eq!(
        roles
            .get(&owner, &org, &rm.id)
            .await
            .unwrap()
            .assignment_count,
        1
    );

    // A direct collaborator grant with the custom role is evaluated too.
    let reader = w.env.forge.hooks.directory.get("reader").await.unwrap();
    w.env
        .forge
        .orgs
        .add_collaborator(&owner, &repo.id, reader, RepoRole::Read, &rm.id)
        .await
        .unwrap();
    let direct = roles.repo_actions(&owner, &repo, "reader").await.unwrap();
    assert!(direct.actions.contains(&Action::ManageRefs));

    // Updating the role changes holders' access at once; deleting falls back to the base role.
    roles
        .update(
            &owner,
            &org,
            &rm.id,
            bee_forge::enterprise::roles::RolePatch {
                permissions: Some(vec!["force_update".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let access = roles.repo_actions(&owner, &repo, "writer").await.unwrap();
    assert!(
        access.actions.contains(&Action::ForceUpdate)
            && !access.actions.contains(&Action::ManageRefs)
    );
    roles.delete(&owner, &org, &rm.id).await.unwrap();
    let access = roles.repo_actions(&owner, &repo, "writer").await.unwrap();
    assert!(access.actions.contains(&Action::Commit));
    assert!(!access.actions.contains(&Action::ForceUpdate));
    let (grants, _) = t
        .team_repos(&owner, &org, "releasers", false, "", 50)
        .await
        .unwrap();
    assert_eq!(
        grants[0].role, "write",
        "holders fall back to the base role"
    );

    // Org roles: permissions held through a principal or a team assignment.
    let billing = roles
        .create(
            &owner,
            &org,
            PolicyRoleKind::Org,
            "Finance",
            "",
            "member",
            vec!["manage_billing".into()],
        )
        .await
        .unwrap();
    assert!(
        !roles
            .has_org_permission(&owner, &org.id, "writer", "manage_billing")
            .await
            .unwrap()
    );
    assert!(
        roles
            .has_org_permission(&owner, &org.id, "admin", "manage_billing")
            .await
            .unwrap(),
        "built-in admins hold it"
    );
    roles
        .assign_org_role(&owner, &org, &billing.id, Some("writer"), None)
        .await
        .unwrap();
    assert!(
        roles
            .has_org_permission(&owner, &org.id, "writer", "manage_billing")
            .await
            .unwrap()
    );
    assert!(
        !roles
            .has_org_permission(&owner, &org.id, "writer", "read_audit_log")
            .await
            .unwrap()
    );
    roles
        .unassign_org_role(&owner, &org, &billing.id, Some("writer"), None)
        .await
        .unwrap();
    assert!(
        !roles
            .has_org_permission(&owner, &org.id, "writer", "manage_billing")
            .await
            .unwrap()
    );
    let team_id = t
        .get(&owner, &org, "releasers", &viewer(&["owner"], true))
        .await
        .unwrap()
        .id;
    roles
        .assign_org_role(&owner, &org, &billing.id, None, Some(&team_id))
        .await
        .unwrap();
    assert!(
        roles
            .has_org_permission(&owner, &org.id, "writer", "manage_billing")
            .await
            .unwrap(),
        "through the team"
    );
    let (assigned, _) = roles
        .list_org_assignments(&owner, &org, &billing.id, "", 50)
        .await
        .unwrap();
    assert_eq!(assigned.len(), 1);
    assert_eq!(
        roles
            .assign_org_role(&owner, &org, &rm.id, Some("writer"), None)
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
    let (listed, _) = roles
        .list(&owner, &org, PolicyRoleKind::Org, "", 50)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
}
on_both_backends!(custom_roles_are_evaluated_through_policy);

async fn review_assignment_picks_deterministically(w: World) {
    let org = make_org(&w.env, "acme").await;
    members(&w, &org).await;
    let t = &w.ent.teams;
    let owner = cx("owner");
    t.create(&owner, &org, new_team("reviewers", ""))
        .await
        .unwrap();
    for p in ["admin", "writer", "reader"] {
        t.add_member(
            &owner,
            &org,
            "reviewers",
            Invitee::Principal(p.into()),
            TeamMemberRole::Member,
            None,
        )
        .await
        .unwrap();
    }
    let none: BTreeMap<String, u32> = BTreeMap::new();
    // Disabled: nobody is assigned.
    assert!(
        t.pick_reviewers(&owner, &org, "reviewers", &[], &none)
            .await
            .unwrap()
            .is_empty()
    );
    let s = t
        .update_review_assignment(
            &owner,
            &org,
            "reviewers",
            ReviewPatch {
                enabled: Some(true),
                member_count: Some(1),
                excluded: Some(vec!["reader".into(), "owner".into()]),
                ..ReviewPatch::default()
            },
        )
        .await
        .unwrap();
    assert!(s.enabled && s.excluded_member_ids == vec!["owner".to_string(), "reader".to_string()]);
    assert_eq!(
        t.update_review_assignment(
            &owner,
            &org,
            "reviewers",
            ReviewPatch {
                member_count: Some(0),
                ..ReviewPatch::default()
            }
        )
        .await
        .unwrap_err()
        .subcode,
        "invalid_setting"
    );

    // Round robin rotates through the eligible members, least recently assigned first.
    let mut picked = Vec::new();
    for _ in 0..4 {
        picked.push(
            t.pick_reviewers(&owner, &org, "reviewers", &[], &none)
                .await
                .unwrap()
                .remove(0),
        );
        w.clock.advance(1000);
    }
    assert_eq!(picked, vec!["admin", "writer", "admin", "writer"]);
    // The author is never assigned their own change.
    let r = t
        .pick_reviewers(&owner, &org, "reviewers", &["admin".to_string()], &none)
        .await
        .unwrap();
    assert_eq!(r, vec!["writer"]);

    // Load balancing prefers the member with the fewest outstanding requests.
    t.update_review_assignment(
        &owner,
        &org,
        "reviewers",
        ReviewPatch {
            algorithm: Some(TeamReviewAlgorithm::LoadBalance),
            member_count: Some(2),
            excluded: Some(vec![]),
            ..ReviewPatch::default()
        },
    )
    .await
    .unwrap();
    let load = BTreeMap::from([
        ("admin".to_string(), 5),
        ("writer".to_string(), 0),
        ("reader".to_string(), 1),
        ("owner".to_string(), 9),
    ]);
    assert_eq!(
        t.pick_reviewers(&owner, &org, "reviewers", &[], &load)
            .await
            .unwrap(),
        vec!["writer", "reader"]
    );
    assert_eq!(
        t.review_assignment(&owner, &org, "reviewers")
            .await
            .unwrap()
            .member_count,
        2
    );
}
on_both_backends!(review_assignment_picks_deterministically);

async fn group_links_make_a_team_synced(w: World) {
    let org = make_org(&w.env, "acme").await;
    members(&w, &org).await;
    let t = &w.ent.teams;
    let owner = cx("owner");
    t.create(&owner, &org, new_team("idp", "")).await.unwrap();
    t.add_member(
        &owner,
        &org,
        "idp",
        Invitee::Principal("reader".into()),
        TeamMemberRole::Member,
        None,
    )
    .await
    .unwrap();
    let g = t
        .link_group(&owner, &org, "idp", "SAML", "engineering", "Engineering")
        .await
        .unwrap();
    assert_eq!(g.provider, "saml");
    assert_eq!(
        t.link_group(&owner, &org, "idp", "saml", "engineering", "")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::AlreadyExists
    );
    assert_eq!(
        t.link_group(&owner, &org, "idp", "a:b", "x", "")
            .await
            .unwrap_err()
            .subcode,
        "invalid_provider"
    );
    assert!(
        t.get(&owner, &org, "idp", &viewer(&["owner"], true))
            .await
            .unwrap()
            .idp_synced
    );
    // Manual changes are refused while the team follows a group.
    for r in [
        t.add_member(
            &owner,
            &org,
            "idp",
            Invitee::Principal("writer".into()),
            TeamMemberRole::Member,
            None,
        )
        .await
        .err(),
        t.remove_member(&owner, &org, "idp", "reader", false)
            .await
            .err(),
    ] {
        assert_eq!(r.unwrap().reason, ErrorReason::InvalidState);
    }
    // The IdP drives membership: a sync adds the group's members and removes only the
    // members that came from a group.
    let want = BTreeMap::from([("writer".to_string(), "saml:engineering".to_string())]);
    let report = t
        .sync(
            &owner,
            &org,
            &t.resolve_id(&owner, &org.id, "idp").await.unwrap(),
            &want,
        )
        .await
        .unwrap();
    assert_eq!(report.added, vec!["writer"]);
    assert!(report.removed.is_empty(), "the manually added member stays");
    let want = BTreeMap::new();
    let report = t
        .sync(
            &owner,
            &org,
            &t.resolve_id(&owner, &org.id, "idp").await.unwrap(),
            &want,
        )
        .await
        .unwrap();
    assert_eq!(report.removed, vec!["writer"]);
    assert!(t.membership(&owner, &org, "idp", "reader").await.is_ok());
    let (groups, _) = t.linked_groups(&owner, &org, "idp", "", 50).await.unwrap();
    assert_eq!(groups.len(), 1);
    assert!(groups[0].last_synced_at.is_some());
    let linked = t
        .teams_linked_to(&owner, "saml", "engineering")
        .await
        .unwrap();
    assert_eq!(linked.len(), 1);
    // Unlinking makes the team manual again; its members stay.
    t.unlink_group(&owner, &org, "idp", "saml", "engineering")
        .await
        .unwrap();
    assert!(
        !t.get(&owner, &org, "idp", &viewer(&["owner"], true))
            .await
            .unwrap()
            .idp_synced
    );
    t.add_member(
        &owner,
        &org,
        "idp",
        Invitee::Principal("writer".into()),
        TeamMemberRole::Member,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        t.unlink_group(&owner, &org, "idp", "saml", "engineering")
            .await
            .unwrap_err()
            .reason,
        ErrorReason::NotFound
    );
}
on_both_backends!(group_links_make_a_team_synced);

fn req<T>(w: &World, who: &str, msg: T) -> tonic::Request<T> {
    common::req(&w.env.full(who), msg)
}

fn status_reason<T: std::fmt::Debug>(r: Result<tonic::Response<T>, tonic::Status>) -> String {
    common::reason(r)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_service_permission_matrix() {
    let w = support::world(support::Backend::Memory).await;
    let org = make_org(&w.env, "acme").await;
    members(&w, &org).await;
    let api = bee_forge::enterprise::service::EnterpriseApi::new(w.ent.clone());
    let repo = make_repo(&w.env, &org, "app").await;
    let create = |slug: &str, parent: &str, privacy: TeamPrivacy| CreateTeamRequest {
        org: "acme".into(),
        slug: slug.into(),
        name: slug.into(),
        parent_team: parent.into(),
        privacy: privacy as i32,
        ..Default::default()
    };

    // Owners and admins create teams; the creator becomes a maintainer.
    let t = api
        .create_team(req(&w, "owner", create("core", "", TeamPrivacy::Visible)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(t.member_count, 1);
    api.create_team(req(&w, "admin", create("ops", "", TeamPrivacy::Visible)))
        .await
        .unwrap();
    // A plain member may create a team only while org settings allow it.
    api.create_team(req(
        &w,
        "reader",
        create("reader-team", "", TeamPrivacy::Visible),
    ))
    .await
    .unwrap();
    w.env
        .forge
        .orgs
        .update_settings(&cx("owner"), &org.id, 0, |s| {
            s.members_can_create_teams = false
        })
        .await
        .unwrap();
    assert_eq!(
        status_reason(
            api.create_team(req(&w, "writer", create("nope", "", TeamPrivacy::Visible)))
                .await
        ),
        "PERMISSION_DENIED"
    );
    // Outsiders are not org members.
    assert_eq!(
        status_reason(
            api.create_team(req(
                &w,
                "outsider",
                create("nope", "", TeamPrivacy::Visible)
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
    // A token limited to reading cannot manage.
    let read_only = common::req(
        &w.env.token("owner", common::actions(&[Action::Read])),
        create("ro", "", TeamPrivacy::Visible),
    );
    assert_eq!(
        status_reason(api.create_team(read_only).await),
        "POLICY_DENIED"
    );
    // No token, no call.
    assert_eq!(
        status_reason(
            api.create_team(tonic::Request::new(create("x", "", TeamPrivacy::Visible)))
                .await
        ),
        "UNAUTHENTICATED"
    );

    // Maintainers manage their team (and children) without being org admins.
    api.add_team_member(req(
        &w,
        "owner",
        AddTeamMemberRequest {
            org: "acme".into(),
            team: "core".into(),
            invitee: Some(
                bee_proto::v1::add_team_member_request::Invitee::PrincipalId("writer".into()),
            ),
            role: TeamMemberRole::Maintainer as i32,
        },
    ))
    .await
    .unwrap();
    api.create_team(req(
        &w,
        "writer",
        create("core-kids", "core", TeamPrivacy::Visible),
    ))
    .await
    .unwrap();
    let add = |who: &str| AddTeamMemberRequest {
        org: "acme".into(),
        team: "core".into(),
        invitee: Some(bee_proto::v1::add_team_member_request::Invitee::PrincipalId(who.into())),
        role: 0,
    };
    api.add_team_member(req(&w, "writer", add("reader")))
        .await
        .unwrap();
    // A plain member of the team cannot.
    assert_eq!(
        status_reason(api.add_team_member(req(&w, "reader", add("admin"))).await),
        "PERMISSION_DENIED"
    );
    // Maintaining `core` does not give a say over the unrelated `ops` team.
    let add_ops = AddTeamMemberRequest {
        team: "ops".into(),
        ..add("reader")
    };
    assert_eq!(
        status_reason(api.add_team_member(req(&w, "writer", add_ops)).await),
        "PERMISSION_DENIED"
    );

    // Secret teams: invisible to non-members.
    api.create_team(req(&w, "owner", create("hush", "", TeamPrivacy::Secret)))
        .await
        .unwrap();
    assert_eq!(
        status_reason(
            api.get_team(req(
                &w,
                "writer",
                GetTeamRequest {
                    org: "acme".into(),
                    team: "hush".into()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert!(
        api.get_team(req(
            &w,
            "owner",
            GetTeamRequest {
                org: "acme".into(),
                team: "hush".into()
            }
        ))
        .await
        .is_ok()
    );
    let listed = api
        .list_teams(req(
            &w,
            "writer",
            ListTeamsRequest {
                org: "acme".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.teams.iter().all(|t| t.slug != "hush"));

    // Repo access needs repo admin: org admins have it, team maintainers do not.
    let grant = |team: &str, role: &str| GrantTeamRepoAccessRequest {
        org: "acme".into(),
        team: team.into(),
        repo: Some(RepoRef {
            repo: Some(repo_ref::Repo::Id(repo.id.clone())),
        }),
        role: role.into(),
    };
    assert_eq!(
        status_reason(
            api.grant_team_repo_access(req(&w, "writer", grant("core", "write")))
                .await
        ),
        "PERMISSION_DENIED"
    );
    let g = api
        .grant_team_repo_access(req(&w, "admin", grant("core", "write")))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(g.role, "write");

    // IdP group links are an org owner/admin function; syncing runs at once.
    let link = LinkTeamExternalGroupRequest {
        org: "acme".into(),
        team: "core".into(),
        provider: "saml".into(),
        group_id: "eng".into(),
    };
    assert_eq!(
        status_reason(
            api.link_team_external_group(req(&w, "writer", link.clone()))
                .await
        ),
        "PERMISSION_DENIED"
    );
    let linked = api
        .link_team_external_group(req(&w, "admin", link))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(linked.group_id, "eng");
    let manual = api.add_team_member(req(&w, "owner", add("admin"))).await;
    assert_eq!(status_reason(manual), "INVALID_STATE");

    // Denied state changes are audited.
    let actions = support::audit_actions(&w, &format!("org:{}", org.id)).await;
    assert!(actions.iter().any(|a| a == "team.create"));
    assert!(actions.iter().any(|a| a == "team.repo_grant"));
    assert!(actions.iter().any(|a| a == "team.group_link"));
}
