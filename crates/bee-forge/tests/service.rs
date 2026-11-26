//! The six forge services called in-process through their tonic traits with tokens of
//! different principals and scopes: the permission matrix, error details, and end-to-end
//! flows for orgs, repos, leases, checks, events and audit.

use crate::common;
use std::time::Duration;

use bee_auth::{Scope, TokenId};
use bee_forge::service::ForgeServices;
use bee_policy::Action;
use bee_proto::v1::audit_service_server::AuditService;
use bee_proto::v1::check_service_server::CheckService;
use bee_proto::v1::event_service_server::EventService;
use bee_proto::v1::lease_service_server::LeaseService;
use bee_proto::v1::org_service_server::OrgService;
use bee_proto::v1::repo_service_server::RepoService;
use bee_proto::v1::*;
use common::{Env, actions, reason, req};
use tokio_stream::StreamExt;

struct World {
    env: Env,
    s: ForgeServices,
    org: Org,
    repo: Repo,
}

fn rref(full: &str) -> Option<RepoRef> {
    Some(RepoRef {
        repo: Some(repo_ref::Repo::FullName(full.into())),
    })
}

async fn world() -> World {
    let env = common::env().await;
    let s = ForgeServices::new(env.forge.clone());
    let owner = env.full("owner");
    let org = s
        .create_org(req(
            &owner,
            CreateOrgRequest {
                name: "acme".into(),
                display: "Acme".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
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
    for (who, role) in [
        ("admin", Role::Admin),
        ("writer", Role::Member),
        ("reader", Role::Member),
        ("ci", Role::Member),
    ] {
        s.set_member(req(
            &owner,
            SetMemberRequest {
                org: "acme".into(),
                principal_id: who.into(),
                role: role as i32,
            },
        ))
        .await
        .unwrap();
    }
    for who in ["writer", "ci"] {
        s.add_collaborator(req(
            &owner,
            AddCollaboratorRequest {
                repo: rref("acme/app"),
                principal_id: who.into(),
                role: RepoRole::Write as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    }
    World { env, s, org, repo }
}

fn lease(paths: &[&str]) -> AcquireLeaseRequest {
    AcquireLeaseRequest {
        repo: rref("acme/app"),
        paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
        mode: LeaseMode::Advisory as i32,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_matrix() {
    let w = world().await;
    let (s, env) = (&w.s, &w.env);
    let owner = env.full("owner");
    let admin = env.full("admin");
    let writer = env.full("writer");
    let reader = env.full("reader");
    let outsider = env.full("outsider");
    let ci = env.full("ci");
    let writer_read_only = env.token("writer", actions(&[Action::Read]));
    let writer_other_repo = env.token("writer", Scope::new().repos(["01OTHERREPO*"]).unwrap());
    let writer_docs_only = env.token("writer", Scope::new().write_paths(["docs"]).unwrap());
    let owner_read_only = env.token("owner", actions(&[Action::Read]));

    let get = |t: &str| {
        s.get_repo(req(
            t,
            GetRepoRequest {
                repo: rref("acme/app"),
            },
        ))
    };
    assert!(get(&owner).await.is_ok());
    assert!(get(&reader).await.is_ok());
    assert!(get(&writer_read_only).await.is_ok());
    assert_eq!(reason(get(&outsider).await), "NOT_FOUND");

    assert!(s.acquire_lease(req(&writer, lease(&["src"]))).await.is_ok());
    assert_eq!(
        reason(s.acquire_lease(req(&reader, lease(&["src"]))).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(s.acquire_lease(req(&outsider, lease(&["src"]))).await),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(
            s.acquire_lease(req(&writer_read_only, lease(&["src"])))
                .await
        ),
        "POLICY_DENIED"
    );
    assert_eq!(
        reason(
            s.acquire_lease(req(&writer_other_repo, lease(&["src"])))
                .await
        ),
        "POLICY_DENIED"
    );
    assert_eq!(
        reason(
            s.acquire_lease(req(&writer_docs_only, lease(&["src"])))
                .await
        ),
        "POLICY_DENIED"
    );
    assert!(
        s.acquire_lease(req(&writer_docs_only, lease(&["docs/a.md"])))
            .await
            .is_ok()
    );
    assert!(
        s.list_leases(req(
            &reader,
            ListLeasesRequest {
                repo: rref("acme/app"),
                paths: vec![],
                org: String::new(),
            }
        ))
        .await
        .is_ok()
    );

    let report = |t: &str| {
        s.report_check(req(
            t,
            ReportCheckRequest {
                repo: rref("acme/app"),
                changeset: Some(ObjectId { raw: vec![2; 33] }),
                name: "build".into(),
                status: CheckStatus::Running as i32,
                ..Default::default()
            },
        ))
    };
    assert!(report(&ci).await.is_ok());
    assert_eq!(reason(report(&reader).await), "PERMISSION_DENIED");

    let settings = |t: &str| {
        s.update_repo_settings(req(
            t,
            UpdateRepoSettingsRequest {
                repo: rref("acme/app"),
                allow_auto_merge: Some(false),
                ..Default::default()
            },
        ))
    };
    assert!(settings(&owner).await.is_ok());
    assert!(settings(&admin).await.is_ok());
    assert_eq!(reason(settings(&writer).await), "PERMISSION_DENIED");
    assert_eq!(reason(settings(&owner_read_only).await), "POLICY_DENIED");

    let hook = |t: &str| {
        s.create_webhook(req(
            t,
            CreateWebhookRequest {
                repo: rref("acme/app"),
                url: "https://hooks.example.com/x".into(),
                ..Default::default()
            },
        ))
    };
    assert!(hook(&owner).await.is_ok());
    assert_eq!(reason(hook(&writer).await), "PERMISSION_DENIED");
    let org_hook = |t: &str| {
        s.create_webhook(req(
            t,
            CreateWebhookRequest {
                org: "acme".into(),
                url: "https://hooks.example.com/o".into(),
                ..Default::default()
            },
        ))
    };
    assert!(org_hook(&admin).await.is_ok());
    assert_eq!(reason(org_hook(&writer).await), "PERMISSION_DENIED");

    let audit = |t: &str| {
        s.query_audit_log(req(
            t,
            QueryAuditLogRequest {
                org: "acme".into(),
                ..Default::default()
            },
        ))
    };
    assert!(audit(&owner).await.is_ok());
    assert!(audit(&admin).await.is_ok());
    assert_eq!(reason(audit(&writer).await), "PERMISSION_DENIED");
    assert_eq!(reason(audit(&outsider).await), "PERMISSION_DENIED");

    let set = |t: &str, who: &str, role: Role| {
        s.set_member(req(
            t,
            SetMemberRequest {
                org: "acme".into(),
                principal_id: who.into(),
                role: role as i32,
            },
        ))
    };
    assert!(set(&admin, "outsider", Role::Member).await.is_ok());
    assert_eq!(
        reason(set(&admin, "outsider", Role::Owner).await),
        "PERMISSION_DENIED"
    );
    assert_eq!(
        reason(set(&writer, "reader", Role::Admin).await),
        "PERMISSION_DENIED"
    );
    assert!(set(&owner, "outsider", Role::Unspecified).await.is_ok());
    assert_eq!(
        reason(set(&owner, "owner", Role::Member).await),
        "INVALID_STATE"
    );

    let del = |t: &str| {
        s.delete_org(req(
            t,
            DeleteOrgRequest {
                org: "acme".into(),
                confirm_name: "acme".into(),
            },
        ))
    };
    assert_eq!(reason(del(&admin).await), "PERMISSION_DENIED");
    assert_eq!(reason(del(&writer).await), "PERMISSION_DENIED");

    // Authentication failures.
    let no_token = tonic::Request::new(GetRepoRequest {
        repo: rref("acme/app"),
    });
    let st = s.get_repo(no_token).await.unwrap_err();
    assert_eq!(
        bee_proto::errors::detail_from_status(&st).unwrap().reason,
        "UNAUTHENTICATED"
    );
    assert_eq!(reason(get("not-a-token").await), "UNAUTHENTICATED");
    let revoked = env
        .ring
        .mint(
            &bee_auth::MintRequest {
                principal: bee_auth::Principal::human("reader", "r"),
                on_behalf_of: vec![],
                scope: Scope::new(),
            },
            env.clock.now_ms() / 1000,
        )
        .unwrap();
    env.revocations.revoke(&TokenId(revoked.id.0.clone()));
    assert_eq!(reason(get(&revoked.text).await), "TOKEN_REVOKED");

    // Served elsewhere.
    assert_eq!(
        reason(
            s.fork_repo(req(
                &owner,
                ForkRepoRequest {
                    repo: rref("acme/app"),
                    ..Default::default()
                }
            ))
            .await
        ),
        "UNIMPLEMENTED"
    );

    // Denied state-changing calls are audited with the reason.
    env.forge.flush().await.unwrap();
    let entries = s
        .query_audit_log(req(
            &owner,
            QueryAuditLogRequest {
                org: "acme".into(),
                filter: Some(AuditLogFilter {
                    action_prefixes: vec!["repo.settings_update".into()],
                    ..Default::default()
                }),
                oldest_first: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .entries;
    let results: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e.actor.as_ref().unwrap().id.as_str(), e.result.as_str()))
        .collect();
    assert!(results.contains(&("owner", "ok")), "{results:?}");
    assert!(
        results.contains(&("writer", "PERMISSION_DENIED")),
        "{results:?}"
    );
    let ok = s
        .verify_audit_chain(req(
            &owner,
            VerifyAuditChainRequest {
                org: "acme".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(ok.ok, "{ok:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_act_within_their_owners_and_their_own_grants() {
    let w = world().await;
    let (s, env) = (&w.s, &w.env);
    let agent = env.agent_token("writer", Scope::new());
    // The chain is [agent, writer]; the agent itself holds no role yet, so the private repo
    // is invisible to it.
    assert_eq!(
        reason(s.acquire_lease(req(&agent, lease(&["src"]))).await),
        "NOT_FOUND"
    );
    s.set_member(req(
        &env.full("owner"),
        SetMemberRequest {
            org: "acme".into(),
            principal_id: "agent".into(),
            role: Role::Member as i32,
        },
    ))
    .await
    .unwrap();
    s.add_collaborator(req(
        &env.full("owner"),
        AddCollaboratorRequest {
            repo: rref("acme/app"),
            principal_id: "agent".into(),
            role: RepoRole::Write as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let got = s
        .acquire_lease(req(&agent, lease(&["src"])))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(got.lease.unwrap().holder.unwrap().id, "agent");
    // An agent of a reader is limited by the reader.
    s.set_member(req(
        &env.full("owner"),
        SetMemberRequest {
            org: "acme".into(),
            principal_id: "writer".into(),
            role: Role::Unspecified as i32,
        },
    ))
    .await
    .unwrap();
    s.remove_collaborator(req(
        &env.full("owner"),
        RemoveCollaboratorRequest {
            repo: rref("acme/app"),
            principal_id: "writer".into(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(s.acquire_lease(req(&agent, lease(&["docs"]))).await),
        "NOT_FOUND"
    );
    env.forge.flush().await.unwrap();
    let entries = s
        .query_audit_log(req(
            &env.full("owner"),
            QueryAuditLogRequest {
                org: "acme".into(),
                filter: Some(AuditLogFilter {
                    actor_id: "agent".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .entries;
    let ok = entries
        .iter()
        .find(|e| e.action == "lease.acquire" && e.result == "ok")
        .expect("audited lease");
    assert_eq!(
        ok.delegation_chain,
        vec!["agent".to_string(), "writer".to_string()]
    );
    assert_eq!(ok.on_behalf_of, "writer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn org_and_repo_lifecycle() {
    let w = world().await;
    let (s, env) = (&w.s, &w.env);
    let owner = env.full("owner");

    // Profile CAS.
    let org = s
        .update_org(req(
            &owner,
            UpdateOrgRequest {
                org: "acme".into(),
                description: Some("d".into()),
                expected_version: w.org.version,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        reason(
            s.update_org(req(
                &owner,
                UpdateOrgRequest {
                    org: "acme".into(),
                    description: Some("e".into()),
                    expected_version: w.org.version,
                    ..Default::default()
                }
            ))
            .await
        ),
        "VERSION_MISMATCH"
    );
    assert_eq!(org.description, "d");

    // Rename repo; old name redirects; ids unchanged.
    let renamed = s
        .update_repo(req(
            &owner,
            UpdateRepoRequest {
                repo: rref("acme/app"),
                name: "service".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (renamed.id.as_str(), renamed.full_name.as_str()),
        (w.repo.id.as_str(), "acme/service")
    );
    let via_old = s
        .get_repo(req(
            &owner,
            GetRepoRequest {
                repo: rref("acme/app"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(via_old.id, w.repo.id);

    // Rename org; old org name redirects for repos too.
    s.rename_org(req(
        &owner,
        RenameOrgRequest {
            org: "acme".into(),
            new_name: "acme-corp".into(),
        },
    ))
    .await
    .unwrap();
    let via_old_org = s
        .get_repo(req(
            &owner,
            GetRepoRequest {
                repo: rref("acme/service"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(via_old_org.full_name, "acme-corp/service");

    // Topics, settings, autolinks, custom properties.
    let topics = s
        .set_repo_topics(req(
            &owner,
            SetRepoTopicsRequest {
                repo: rref("acme-corp/service"),
                topics: vec!["Rust".into(), "vcs".into(), "rust".into()],
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(topics.topics, vec!["rust", "vcs"]);
    assert_eq!(
        reason(
            s.set_repo_topics(req(
                &owner,
                SetRepoTopicsRequest {
                    repo: rref("acme-corp/service"),
                    topics: vec!["Bad Topic".into()]
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let st = s
        .get_repo_settings(req(
            &owner,
            GetRepoSettingsRequest {
                repo: rref("acme-corp/service"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(st.git_enabled);
    assert_eq!(
        reason(
            s.update_repo_settings(req(
                &owner,
                UpdateRepoSettingsRequest {
                    repo: rref("acme-corp/service"),
                    set_allowed_land_methods: true,
                    allowed_land_methods: vec![LandMethod::Squash as i32],
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    let st2 = s
        .update_repo_settings(req(
            &owner,
            UpdateRepoSettingsRequest {
                repo: rref("acme-corp/service"),
                set_allowed_land_methods: true,
                allowed_land_methods: vec![LandMethod::Squash as i32],
                default_land_method: Some(LandMethod::Squash as i32),
                expected_version: st.version,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(st2.version, st.version + 1);
    s.create_autolink(req(
        &owner,
        CreateAutolinkRequest {
            repo: rref("acme-corp/service"),
            key_prefix: "JIRA-".into(),
            url_template: "https://j/<num>".into(),
            alphanumeric: false,
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            s.create_autolink(req(
                &owner,
                CreateAutolinkRequest {
                    repo: rref("acme-corp/service"),
                    key_prefix: "jira-".into(),
                    url_template: "https://j/<num>".into(),
                    alphanumeric: false
                }
            ))
            .await
        ),
        "ALREADY_EXISTS"
    );
    s.set_custom_property_definition(req(
        &owner,
        SetCustomPropertyDefinitionRequest {
            org: "acme-corp".into(),
            definition: Some(CustomPropertyDefinition {
                name: "tier".into(),
                r#type: CustomPropertyType::SingleSelect as i32,
                allowed_values: vec!["gold".into(), "silver".into()],
                default_value: "silver".into(),
                ..Default::default()
            }),
        },
    ))
    .await
    .unwrap();
    let props = s
        .get_repo_custom_properties(req(
            &owner,
            GetRepoCustomPropertiesRequest {
                repo: rref("acme-corp/service"),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        props.properties,
        vec![KeyValue {
            key: "tier".into(),
            value: "silver".into()
        }]
    );
    assert_eq!(
        reason(
            s.set_repo_custom_properties(req(
                &owner,
                SetRepoCustomPropertiesRequest {
                    repo: rref("acme-corp/service"),
                    properties: vec![KeyValue {
                        key: "tier".into(),
                        value: "bronze".into()
                    }]
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    s.set_repo_custom_properties(req(
        &owner,
        SetRepoCustomPropertiesRequest {
            repo: rref("acme-corp/service"),
            properties: vec![KeyValue {
                key: "tier".into(),
                value: "gold".into(),
            }],
        },
    ))
    .await
    .unwrap();

    // Outside collaborator through an invitation.
    let outsider = env.full("outsider");
    let added = s
        .add_collaborator(req(
            &owner,
            AddCollaboratorRequest {
                repo: rref("acme-corp/service"),
                principal_id: "outsider".into(),
                role: RepoRole::Read as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let inv = added.invitation.expect("non-members are invited");
    assert_eq!(
        reason(
            s.get_repo(req(
                &outsider,
                GetRepoRequest {
                    repo: rref("acme-corp/service")
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let mine = s
        .list_repo_invitations(req(&outsider, ListRepoInvitationsRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(mine.invitations.len(), 1);
    s.accept_repo_invitation(req(
        &outsider,
        AcceptRepoInvitationRequest {
            invitation_id: inv.id.clone(),
        },
    ))
    .await
    .unwrap();
    assert!(
        s.get_repo(req(
            &outsider,
            GetRepoRequest {
                repo: rref("acme-corp/service")
            }
        ))
        .await
        .is_ok()
    );
    let perm = s
        .get_collaborator_permission(req(
            &owner,
            GetCollaboratorPermissionRequest {
                repo: rref("acme-corp/service"),
                principal_id: "outsider".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (perm.role, perm.sources.clone()),
        (RepoRole::Read as i32, vec!["direct".to_string()])
    );
    let oc = s
        .list_outside_collaborators(req(
            &owner,
            ListOutsideCollaboratorsRequest {
                org: "acme-corp".into(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(oc.collaborators.len(), 1);

    // Org invitation for someone by login.
    let oi = s
        .create_org_invitation(req(
            &owner,
            CreateOrgInvitationRequest {
                org: "acme-corp".into(),
                invitee_login: "outsider-login".into(),
                role: Role::Member as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        reason(
            s.accept_org_invitation(req(
                &env.full("reader"),
                AcceptOrgInvitationRequest {
                    invitation_id: oi.id.clone()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let m = s
        .accept_org_invitation(req(
            &outsider,
            AcceptOrgInvitationRequest {
                invitation_id: oi.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(m.role, Role::Member as i32);
    assert_eq!(
        reason(
            s.accept_org_invitation(req(
                &outsider,
                AcceptOrgInvitationRequest {
                    invitation_id: oi.id
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );

    // Archive makes writes fail with ARCHIVED.
    s.archive_repo(req(
        &owner,
        ArchiveRepoRequest {
            repo: rref("acme-corp/service"),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            s.acquire_lease(req(
                &env.full("writer"),
                AcquireLeaseRequest {
                    repo: rref("acme-corp/service"),
                    paths: vec![b"x".to_vec()],
                    ..Default::default()
                }
            ))
            .await
        ),
        "ARCHIVED"
    );
    s.unarchive_repo(req(
        &owner,
        UnarchiveRepoRequest {
            repo: rref("acme-corp/service"),
        },
    ))
    .await
    .unwrap();

    // Delete, list deleted, restore; name released meanwhile.
    assert_eq!(
        reason(
            s.delete_repo(req(
                &owner,
                DeleteRepoRequest {
                    repo: rref("acme-corp/service"),
                    confirm_full_name: "wrong".into()
                }
            ))
            .await
        ),
        "INVALID_ARGUMENT"
    );
    s.delete_repo(req(
        &owner,
        DeleteRepoRequest {
            repo: rref("acme-corp/service"),
            confirm_full_name: "acme-corp/service".into(),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        reason(
            s.get_repo(req(
                &owner,
                GetRepoRequest {
                    repo: rref("acme-corp/service")
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let deleted = s
        .list_deleted_repos(req(
            &owner,
            ListDeletedReposRequest {
                org: "acme-corp".into(),
                page: None,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(deleted.repos.len(), 1);
    let restored = s
        .restore_repo(req(
            &owner,
            RestoreRepoRequest {
                repo_id: w.repo.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(restored.deleted_at.is_none());

    // Transfer to another org keeps the id and redirects.
    s.create_org(req(
        &owner,
        CreateOrgRequest {
            name: "other".into(),
            display: String::new(),
        },
    ))
    .await
    .unwrap();
    let moved = s
        .transfer_repo(req(
            &owner,
            TransferRepoRequest {
                repo: rref("acme-corp/service"),
                new_org: "other".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (moved.id.as_str(), moved.full_name.as_str()),
        (w.repo.id.as_str(), "other/service")
    );
    assert_eq!(
        s.get_repo(req(
            &owner,
            GetRepoRequest {
                repo: rref("acme-corp/service")
            }
        ))
        .await
        .unwrap()
        .into_inner()
        .id,
        w.repo.id
    );

    // Templates.
    s.update_repo(req(
        &owner,
        UpdateRepoRequest {
            repo: rref("other/service"),
            is_template: Some(true),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let from = s
        .create_repo_from_template(req(
            &owner,
            CreateRepoFromTemplateRequest {
                template: rref("other/service"),
                org: "other".into(),
                name: "copy".into(),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(from.template_repo_id, w.repo.id);
    assert_eq!(
        reason(
            s.create_repo_from_template(req(
                &owner,
                CreateRepoFromTemplateRequest {
                    template: rref("other/copy"),
                    org: "other".into(),
                    name: "copy2".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "INVALID_STATE"
    );

    // Org delete and restore.
    let gone = s
        .delete_org(req(
            &owner,
            DeleteOrgRequest {
                org: "other".into(),
                confirm_name: "other".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(gone.restorable_until.is_some());
    assert_eq!(
        reason(
            s.get_org(req(
                &owner,
                GetOrgRequest {
                    name_or_id: "other".into()
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    assert_eq!(
        reason(
            s.get_repo(req(
                &owner,
                GetRepoRequest {
                    repo: rref("other/service")
                }
            ))
            .await
        ),
        "NOT_FOUND"
    );
    let other_id = env
        .forge
        .orgs
        .orgs_of(&bee_forge::Cx::system(), "owner", "", 10)
        .await
        .unwrap()
        .0;
    assert_eq!(other_id.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_statuses_and_required_check_input() {
    let w = world().await;
    let (s, env) = (&w.s, &w.env);
    let ci = env.full("ci");
    let cs = ObjectId { raw: vec![5; 33] };
    let suite = s
        .create_check_suite(req(
            &ci,
            CreateCheckSuiteRequest {
                repo: rref("acme/app"),
                changeset: Some(cs.clone()),
                head_ref: "main".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let run = s
        .report_check(req(
            &ci,
            ReportCheckRequest {
                repo: rref("acme/app"),
                changeset: Some(cs.clone()),
                name: "test".into(),
                status: CheckStatus::Running as i32,
                suite_id: suite.id.clone(),
                output: Some(CheckOutput {
                    title: "t".into(),
                    annotations: vec![CheckAnnotation {
                        path: b"src/a.rs".to_vec(),
                        start_line: 1,
                        end_line: 2,
                        level: AnnotationLevel::Warning as i32,
                        message: "m".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(run.started_at.is_some() && run.finished_at.is_none());
    let done = s
        .update_check_run(req(
            &ci,
            UpdateCheckRunRequest {
                repo: rref("acme/app"),
                id: run.id.clone(),
                status: Some(CheckStatus::Failed as i32),
                output: Some(CheckOutput {
                    annotations: vec![CheckAnnotation {
                        path: b"src/b.rs".to_vec(),
                        message: "n".into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (done.annotations_count, done.finished_at.is_some()),
        (2, true)
    );
    let ann = s
        .list_check_annotations(req(
            &ci,
            ListCheckAnnotationsRequest {
                repo: rref("acme/app"),
                check_run_id: run.id.clone(),
                page: Some(PageRequest {
                    page_size: 1,
                    page_token: String::new(),
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ann.annotations.len(), 1);
    let ann2 = s
        .list_check_annotations(req(
            &ci,
            ListCheckAnnotationsRequest {
                repo: rref("acme/app"),
                check_run_id: run.id.clone(),
                page: Some(PageRequest {
                    page_size: 5,
                    page_token: ann.page.unwrap().next_page_token,
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(ann2.annotations[0].message, "n");
    let suites = s
        .list_check_suites(req(
            &ci,
            ListCheckSuitesRequest {
                repo: rref("acme/app"),
                changeset: Some(cs.clone()),
                app_id: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(suites.suites[0].status, CheckStatus::Failed as i32);

    // Rerequest queues a fresh run; latest-per-name sees it.
    let again = s
        .rerequest_check_run(req(
            &ci,
            RerequestCheckRunRequest {
                repo: rref("acme/app"),
                id: run.id.clone(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again.status, CheckStatus::Queued as i32);
    let latest = s
        .list_checks(req(
            &ci,
            ListChecksRequest {
                repo: rref("acme/app"),
                changeset: Some(cs.clone()),
                latest_only: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(latest.checks.len(), 1);
    assert_eq!(latest.checks[0].id, again.id);

    // Commit statuses map onto check runs and combine.
    for (ctx, st) in [
        ("lint", CommitStatusState::Success),
        ("deploy", CommitStatusState::Pending),
    ] {
        s.create_commit_status(req(
            &ci,
            CreateCommitStatusRequest {
                repo: rref("acme/app"),
                changeset: Some(cs.clone()),
                context: ctx.into(),
                state: st as i32,
                ..Default::default()
            },
        ))
        .await
        .unwrap();
    }
    let hexcs: String = cs.raw.iter().map(|b| format!("{b:02x}")).collect();
    let combined = s
        .get_combined_status(req(
            &ci,
            GetCombinedStatusRequest {
                repo: rref("acme/app"),
                at: Some(Revision {
                    expr: hexcs.clone(),
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (combined.state, combined.statuses.len()),
        (CommitStatusState::Pending as i32, 2)
    );
    s.create_commit_status(req(
        &ci,
        CreateCommitStatusRequest {
            repo: rref("acme/app"),
            changeset: Some(cs.clone()),
            context: "deploy".into(),
            state: CommitStatusState::Success as i32,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let combined = s
        .get_combined_status(req(
            &ci,
            GetCombinedStatusRequest {
                repo: rref("acme/app"),
                at: Some(Revision {
                    expr: hexcs.clone(),
                }),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(combined.state, CommitStatusState::Success as i32);
    let all = s
        .list_checks(req(
            &ci,
            ListChecksRequest {
                repo: rref("acme/app"),
                changeset: Some(cs.clone()),
                latest_only: true,
                ..Default::default()
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mirrored = all.checks.iter().find(|c| c.name == "deploy").unwrap();
    assert_eq!(
        (mirrored.status, mirrored.external_id.as_str()),
        (CheckStatus::Passed as i32, "status:deploy")
    );

    // Required-check input for policy.
    let results = env
        .forge
        .checks
        .check_results(&bee_forge::Cx::system(), &w.repo.id, &hexcs)
        .await
        .unwrap();
    let get = |n: &str| results.iter().find(|r| r.name == n).unwrap().status;
    assert_eq!(get("test"), bee_policy::CheckStatus::Pending);
    assert_eq!(get("lint"), bee_policy::CheckStatus::Success);

    // Post-land failures feed bisect.
    s.report_check(req(
        &ci,
        ReportCheckRequest {
            repo: rref("acme/app"),
            changeset: Some(cs.clone()),
            name: "smoke".into(),
            status: CheckStatus::Failed as i32,
            post_land: true,
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    let failing = env
        .forge
        .checks
        .failing_post_land(&bee_forge::Cx::system(), &w.repo.id, &hexcs)
        .await
        .unwrap();
    assert_eq!(failing.len(), 1);
    assert_eq!(
        reason(
            s.start_bisect(req(&ci, StartBisectRequest::default()))
                .await
        ),
        "UNIMPLEMENTED"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_subscriptions_of_one_credential_share_their_access_decisions() {
    const STREAMS: usize = 40;
    let w = world().await;
    let (s, env) = (&w.s, &w.env);
    let (owner, writer) = (env.full("owner"), env.full("writer"));
    env.forge.flush().await.unwrap();
    let latest = s
        .get_event_log_info(req(
            &writer,
            GetEventLogInfoRequest {
                repo: rref("acme/app"),
                org: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner()
        .latest_seq;
    let mut streams = Vec::new();
    for _ in 0..STREAMS {
        streams.push(
            s.subscribe(req(
                &writer,
                SubscribeRequest {
                    repo: rref("acme/app"),
                    from_seq: latest + 1,
                    kinds: vec!["lease.*".into()],
                    org: String::new(),
                },
            ))
            .await
            .unwrap()
            .into_inner(),
        );
    }
    // Idle for a little over three seconds: one decision a second between them, where each
    // stream deciding for itself would make about three each.
    let before = s.stream_access_decisions();
    tokio::time::sleep(Duration::from_millis(3300)).await;
    let made = s.stream_access_decisions() - before;
    assert!(
        (2..=6).contains(&made),
        "{STREAMS} idle streams of one credential made {made} access decisions in 3.3 s"
    );
    // Every stream still gets every event.
    s.acquire_lease(req(&writer, lease(&["src"])))
        .await
        .unwrap();
    env.forge.flush().await.unwrap();
    for stream in &mut streams {
        let e = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the event reaches every stream")
            .unwrap()
            .unwrap();
        assert_eq!(e.kind, "lease.acquired");
    }
    // And losing access ends every one of them with the refusal.
    s.remove_collaborator(req(
        &owner,
        RemoveCollaboratorRequest {
            repo: rref("acme/app"),
            principal_id: "writer".into(),
        },
    ))
    .await
    .unwrap();
    s.set_member(req(
        &owner,
        SetMemberRequest {
            org: "acme".into(),
            principal_id: "writer".into(),
            role: Role::Unspecified as i32,
        },
    ))
    .await
    .unwrap();
    let ended = tokio::time::Instant::now() + Duration::from_secs(4);
    for stream in &mut streams {
        let status = tokio::time::timeout_at(ended, stream.next())
            .await
            .expect("every stream ends within the re-authorization bound")
            .expect("the stream reports why it ended")
            .expect_err("a caller without access gets no more events");
        let detail = bee_proto::errors::detail_from_status(&status).expect("typed status");
        assert_eq!(detail.reason, "NOT_FOUND");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_and_page_events_through_the_service() {
    let w = world().await;
    let (s, env) = (&w.s, &w.env);
    let writer = env.full("writer");
    env.forge.flush().await.unwrap();
    let info = s
        .get_event_log_info(req(
            &writer,
            GetEventLogInfoRequest {
                repo: rref("acme/app"),
                org: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mut stream = s
        .subscribe(req(
            &writer,
            SubscribeRequest {
                repo: rref("acme/app"),
                from_seq: info.latest_seq + 1,
                kinds: vec!["lease.*".into()],
                org: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    s.acquire_lease(req(&writer, lease(&["src"])))
        .await
        .unwrap();
    env.forge.flush().await.unwrap();
    let e = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(e.kind, "lease.acquired");
    assert_eq!(e.principal, "writer");
    let page = s
        .list_events(req(
            &writer,
            ListEventsRequest {
                repo: rref("acme/app"),
                from_seq: 0,
                kinds: vec!["repo.*".into()],
                limit: 10,
                org: String::new(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.events[0].kind, "repo.created");
    let bad = s
        .subscribe(req(
            &writer,
            SubscribeRequest {
                repo: rref("acme/app"),
                kinds: vec!["nope".into()],
                ..Default::default()
            },
        ))
        .await;
    let st = bad.err().expect("unknown kind refused");
    assert_eq!(
        bee_proto::errors::detail_from_status(&st).unwrap().reason,
        "INVALID_ARGUMENT"
    );
    // Org log: members only.
    assert!(
        s.list_events(req(
            &writer,
            ListEventsRequest {
                org: "acme".into(),
                ..Default::default()
            }
        ))
        .await
        .is_ok()
    );
    assert_eq!(
        reason(
            s.list_events(req(
                &env.full("outsider"),
                ListEventsRequest {
                    org: "acme".into(),
                    ..Default::default()
                }
            ))
            .await
        ),
        "PERMISSION_DENIED"
    );
}
