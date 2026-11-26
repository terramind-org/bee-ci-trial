//! Cancellation racing an org join must never grant a cancelled team invitation.

use std::sync::Arc;

use crate::common;
use crate::enterprise_support::{cx, make_org};
use bee_forge::enterprise::network::{MemoryTwoFactor, StaticResolver};
use bee_forge::enterprise::teams::{AddOutcome, Invitee, NewTeam};
use bee_forge::enterprise::{Enterprise, EnterpriseConfig};
use bee_forge::hooks::PrincipalDirectory;
use bee_forge::ids::SystemClock;
use bee_forge::store::ForgeStore;
use bee_forge::store::sql::{SqlBackend, SqlConfig, SqlStore};
use bee_proto::v1::{ErrorReason, Principal, PrincipalKind, Role, TeamMemberRole};
use tokio::sync::Barrier;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_cancelled_team_invitation_grants_nothing_when_configured() {
    let Ok(url) = std::env::var("BEE_TEST_PG") else {
        eprintln!("BEE_TEST_PG not set; skipping Postgres");
        return;
    };

    let mut cfg = SqlConfig::parse(&url).unwrap();
    if let SqlBackend::Postgres { schema, .. } = &mut cfg.backend {
        *schema = format!("forge_team_invite_race_{}", std::process::id());
    }
    let first_store: Arc<dyn ForgeStore> = Arc::new(
        SqlStore::open(cfg.clone(), Arc::new(SystemClock))
            .await
            .unwrap(),
    );
    let second_store: Arc<dyn ForgeStore> =
        Arc::new(SqlStore::open(cfg, Arc::new(SystemClock)).await.unwrap());
    let first = common::env_with(first_store, Arc::new(SystemClock)).await;
    let second = common::env_with(second_store, Arc::new(SystemClock)).await;
    let first_ent = Enterprise::new(
        first.forge.clone(),
        EnterpriseConfig {
            base_url: "https://hub.example.com".into(),
            dns: Arc::new(StaticResolver::new()),
            two_factor: Arc::new(MemoryTwoFactor::new()),
        },
    );
    first_ent.attach();
    let second_ent = Enterprise::new(
        second.forge.clone(),
        EnterpriseConfig {
            base_url: "https://hub.example.com".into(),
            dns: Arc::new(StaticResolver::new()),
            two_factor: Arc::new(MemoryTwoFactor::new()),
        },
    );
    second_ent.attach();

    let org = make_org(&first, "team-invite-race").await;
    first_ent
        .teams
        .create(
            &cx("owner"),
            &org,
            NewTeam {
                slug: "core".into(),
                name: "core".into(),
                maintainers: vec!["owner".into()],
                ..NewTeam::default()
            },
        )
        .await
        .unwrap();

    for round in 0..300 {
        let invitee = format!("invitee-{round}");
        let principal = Principal {
            id: invitee.clone(),
            kind: PrincipalKind::Human as i32,
            display: invitee.clone(),
            ..Default::default()
        };
        for env in [&first, &second] {
            env.directory.register(
                principal.clone(),
                vec![format!("{invitee}-login")],
                vec![format!("{invitee}@example.com")],
            );
        }
        let AddOutcome::Invitation(invitation) = first_ent
            .teams
            .add_member(
                &cx("owner"),
                &org,
                "core",
                Invitee::Principal(invitee.clone()),
                TeamMemberRole::Member,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("non-org member must receive a pending invitation");
        };

        let barrier = Arc::new(Barrier::new(2));
        let cancel_teams = first_ent.teams.clone();
        let cancel_org = org.clone();
        let cancel_id = invitation.id.clone();
        let cancel_cx = cx("owner");
        let cancel_barrier = barrier.clone();
        let join_forge = second.forge.clone();
        let join_directory = second.directory.clone();
        let join_org = org.clone();
        let join_invitee = invitee.clone();
        let join_barrier = barrier;
        let (cancelled, joined) = tokio::join!(
            async move {
                cancel_barrier.wait().await;
                cancel_teams
                    .cancel_invitation(&cancel_cx, &cancel_org, "core", &cancel_id)
                    .await
            },
            async move {
                join_barrier.wait().await;
                let person = join_directory.get(&join_invitee).await.unwrap();
                join_forge
                    .orgs
                    .set_member(&cx("owner"), &join_org.id, person, Role::Member)
                    .await?;
                join_forge.flush().await
            },
        );
        assert!(
            joined.is_ok(),
            "org join failed in round {round}: {joined:?}"
        );
        assert!(
            cancelled
                .as_ref()
                .err()
                .is_none_or(|error| error.reason == ErrorReason::NotFound),
            "unexpected cancellation error in round {round}: {cancelled:?}"
        );
        let member = first_ent
            .teams
            .membership(&cx("owner"), &org, "core", &invitee)
            .await;
        let (invitations, _) = first_ent
            .teams
            .invitations(&cx("owner"), &org, "core", "", 100)
            .await
            .unwrap();
        let invitation_exists = invitations.iter().any(|i| i.id == invitation.id);
        let became_member = member.is_ok();
        assert!(
            !(cancelled.is_ok() && became_member),
            "successful cancellation granted membership in round {round}"
        );
        assert!(
            cancelled.is_ok() ^ became_member,
            "exactly one terminal outcome required in round {round}: cancel={cancelled:?}, member={member:?}"
        );
        assert!(
            !invitation_exists,
            "invitation remained after join/cancel in round {round}"
        );
    }
}
